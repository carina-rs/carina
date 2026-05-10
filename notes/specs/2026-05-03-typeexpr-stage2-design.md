# Stage 2 of #2360 — Drop `Option<TypeExpr>` for `ExportParameter`

<!-- derived-from ./2026-05-03-typeexpr-stage1-summary.md -->
<!-- constrained-by https://github.com/carina-rs/carina/issues/2360 -->

## Goal

Eliminate `Option<TypeExpr>` from `ExportParameter` so every consumer of
a parsed configuration sees a `TypeExpr`-confirmed export. Stage 1
(#2361 / PR #2362) implemented rhs-driven inference and wired it into
the three validation surfaces (CLI validate, LSP diagnostics, LSP
completion); stage 2 closes the field by removing the optional wrapper
entirely so that "post-load `None`" becomes representable only as a
hard error, never as silent skip.

`AttributeParameter` and `TypedAttributeParam` keep `Option<TypeExpr>`
in this stage. Their treatment is deferred to a follow-up PR (call it
stage 3) tracked from #2360 — they touch module signatures, plan, and
LSP attribute paths, all of which would balloon the diff and dilute
review focus.

## Chosen approach

The combined approach is **C2c (generic) + D2 (loader-side inference)
+ E2 with sentinel (error-accumulating, sentinel-preserving)**. Each
piece is independently selected; together they form a single
end-to-end design.

### Type-state via generic `File<E>`

`ParsedFile` becomes `pub type ParsedFile = File<ParsedExportParam>;`
where `File<E>` is a single generic struct. The exports field changes
shape with `E`:

```rust
pub struct File<E> {
    pub providers: Vec<ProviderConfig>,
    pub resources: Vec<Resource>,
    pub variables: IndexMap<String, Value>,
    // ... every other field unchanged ...
    pub export_params: Vec<E>,
}

pub type ParsedFile = File<ParsedExportParam>;
pub type InferredFile = File<InferredExportParam>;

pub struct ParsedExportParam {
    pub name: String,
    pub type_expr: Option<TypeExpr>,
    pub value: Option<Value>,
}

pub struct InferredExportParam {
    pub name: String,
    pub type_expr: TypeExpr,
    pub value: Option<Value>,
}
```

Exactly one field gets the type-parameter treatment; the rest of the
struct is shared by both phases. Type aliases mean every existing
`ParsedFile` reference in parser/resolve code paths continues to
compile unchanged.

`apply_inference(parsed: ParsedFile, schemas: &SchemaRegistry) ->
(InferredFile, Vec<InferenceError>)` is the single transition. It walks
`parsed.export_params`, runs the existing stage-1 `infer_type_expr`
helper on each, and produces an `InferredExportParam` per entry —
either with the inferred/declared `TypeExpr` or with the
`TypeExpr::Unknown` sentinel (see below) when inference fails.

### Loader-side inference (D2)

`load_configuration_with_config(path, ctx)` already runs parse +
resolve and returns a `LoadedConfig { parsed: ParsedFile, ... }`. Stage
2 extends it to:

- accept an additional `&SchemaRegistry` argument,
- run `apply_inference` after resolve completes,
- return `LoadedConfig { parsed: InferredFile, ..., inference_errors:
  Vec<InferenceError> }`.

CLI (`carina-cli/src/commands/...`) and LSP
(`carina-lsp/src/diagnostics/mod.rs`, completion paths) already hold
`SchemaRegistry` at the loader call site, so threading the new argument
is a one-line per call site change.

Parser tests that call `parse_directory` directly (the low-level entry
point, not `load_configuration`) remain unaffected — they continue
producing `ParsedFile`. Tests that need to exercise inference flip to
calling `apply_inference` explicitly.

### `TypeExpr::Unknown` sentinel for failed inference (E2)

Inference failures are accumulated rather than surfaced as `LoadError`
to preserve the project's "report all errors at once" pattern (#2102 /
#2105). `LoadedConfig.inference_errors` carries the list:

```rust
pub struct LoadedConfig {
    pub parsed: InferredFile,
    pub identifier_scope_errors: Vec<ParseError>,
    pub inference_errors: Vec<InferenceError>,  // new
    // ... other fields unchanged ...
}
```

A failed-inference export still appears in `parsed.export_params` with
its `type_expr` set to the new sentinel `TypeExpr::Unknown`. The
sentinel exists for two reasons:

1. Downstream checks that walk `parsed.export_params` (binding-graph,
   for-iterable shape, etc.) keep finding the export by name and don't
   spawn cascading "missing export" diagnostics.
2. Type-comparison predicates treat `TypeExpr::Unknown` as
   "incompatible with everything" so a downstream `vpc_id =
   network.vpc_id` against a `Custom{VpcId}` receiver still gets a
   diagnostic — phrased as "type annotation required upstream" via the
   existing inference-error message, not "missing export".

`TypeExpr::Unknown` is a *production* sentinel, intentionally
distinguishable from inference omission. Existing
`is_type_expr_compatible_with_schema` returns `false` for `Unknown`
against any concrete receiver, falling through the existing predicate
arms. The sentinel never appears in user-written DSL.

## Key design decisions

- **Generic over the exports field only.** `File<E>` parameterizes a
  single field rather than introducing a phase enum or PhantomData. The
  field's element type changes shape between phases; everything else is
  identical. A future stage (`AttributeParameter`) extends to
  `File<E, A>` orthogonally.
- **Inference at loader, not at parser.** Keeps the parser pure and
  free of `SchemaRegistry`. The loader is the natural pipeline stage
  that already orchestrates parse → resolve.
- **Sentinel over exclusion.** Excluding failed exports from
  `parsed.export_params` would cascade into "unknown export"
  diagnostics; the sentinel localizes the failure to its single
  inference error.
- **Stage 3 is a separate issue.** `AttributeParameter` and
  `TypedAttributeParam` touch module signatures — the same generic
  pattern (`File<E, A>`) extends them, but the call sites that get
  cut over (plan, apply, module call resolution) merit a focused PR.

## File structure / architecture

Files touched (estimate; final count pinned in plan):

| File | Change |
| --- | --- |
| `carina-core/src/parser/ast.rs` | `ParsedFile` → `File<E>` generic; `ExportParameter` → `ParsedExportParam`; new `InferredExportParam`; new `TypeExpr::Unknown` variant; type aliases for `ParsedFile` / `InferredFile` |
| `carina-core/src/validation/inference.rs` | Add `apply_inference(parsed: ParsedFile, schemas: &SchemaRegistry) -> (InferredFile, Vec<InferenceError>)` |
| `carina-core/src/validation/mod.rs` | `validate_export_param_ref_types` consumes `&[InferredExportParam]`; the inline `Option` unwrap goes away (the `type_expr` field is now bare `TypeExpr`) |
| `carina-core/src/config_loader.rs` | `load_configuration_with_config` gains `&SchemaRegistry`; calls `apply_inference`; `LoadedConfig` gains `inference_errors` field |
| `carina-core/src/upstream_exports.rs` | `resolve_upstream_exports_with_schemas` already takes schemas; the inner `e.type_expr.clone()` call adapts to the new bare `TypeExpr` |
| `carina-cli/src/commands/mod.rs`, `plan.rs` | Loader call site adds schema arg; `is_some/clone` guards on `param.type_expr` collapse |
| `carina-lsp/src/diagnostics/checks.rs`, `completion/values.rs` | Same: loader call adds schema; `Option`-handling guards collapse |
| `carina-core/src/validation/tests.rs` and others | Existing tests that build `ExportParameter { type_expr: Some(...) }` change to `ParsedExportParam` (still optional for stage-1 fixtures) or `InferredExportParam` (for stage-2 post-inference fixtures) |

`TypeExpr::Unknown` lives in `carina-core/src/parser/ast.rs` next to
the other `TypeExpr` variants, marked with a doc-comment explaining
its sentinel role and that it must never be produced by the parser.

## Edge cases and constraints

- **Parser tests using `Some(TypeExpr::String)` annotations directly**
  continue to work: they exercise `ParsedExportParam` (the
  `type_expr: Option<TypeExpr>` shape).
- **LSP partial buffers** (mid-edit, missing `}` etc.) flow through
  `parse_directory`, not `load_configuration`, on most diagnostic
  paths. Where they do go through `load_configuration`, an inference
  error becomes a soft diagnostic alongside other parse-time errors,
  matching today's "report all errors" semantics.
- **Cross-repo callers** (`carina-provider-aws`, `carina-provider-awscc`):
  none call `load_configuration` directly; both depend on
  `carina-core`'s public API only via `Provider` / `ProviderFactory`
  traits, not via `ParsedFile`. No coordination concern.
- **Plan/apply flows** consume `parsed.export_params` to compute the
  export diff for `carina export` and the upstream snapshot for `carina
  apply`. After stage 2 they read `Vec<InferredExportParam>` whose
  `type_expr` is bare `TypeExpr` (or `Unknown`); the diff/serialize
  logic does not currently look at `type_expr`, so no behavior change
  there.
- **State serialization**: `ParsedFile` is not serialized (the parser
  produces it from source on every run); `Resource` and `State` are
  serialized but neither carries `ExportParameter`, so no migration.
- **`TypeExpr::Unknown` and `is_type_expr_compatible_with_schema`**:
  the predicate's existing match arms (`String`, `Bool`, `Int`,
  `Float`, `Simple`, `List`, `Map`, `Struct`, fallthrough `Ref`/
  `SchemaType`) cover today's variants. `Unknown` is added as an
  explicit arm returning `false` for any receiver, with a comment
  pointing back to the sentinel's design.
- **PartialEq on `TypeExpr`**: derived; `Unknown == Unknown` is true.
  This is harmless because the sentinel never appears outside
  inference-failure paths and the predicate path explicitly rejects it
  before equality checks matter.

## Out of scope

- `AttributeParameter.type_expr` and `TypedAttributeParam.type_expr`
  cleanup (stage 3 — separate issue).
- Recursive inference through `upstream_state` bindings (#2357 — a
  separate follow-up that #2361 / #2362 deferred).
- Schema-ing `Any`-returning builtins (`lookup`, `min`, `max`, `map`)
  so they too become inferable instead of demanding annotation
  (#2360 long-term direction).

## Acceptance

- `grep -rn "Option<TypeExpr>" carina-core/src/parser/ast.rs` returns
  no hits on `ExportParameter`'s field.
- `grep -rn "param.type_expr.is_some\|param.type_expr.is_none" carina-core/src carina-cli/src carina-lsp/src` returns no hits on
  export-param paths (`AttributeParameter` paths still use them).
- The reproducer `~/tmp/carina-upstream-state/` continues to validate
  cleanly with the unannotated `vpc_id = main.vpc_id` shape; an
  unannotated `zone_id = lookup(...)` produces "type annotation
  required" via the new `inference_errors` channel.
- 27 unit tests + 4 e2e tests from stage 1 continue to pass.
- New tests cover: `apply_inference` happy path, sentinel preservation
  on failure, loader integration, `TypeExpr::Unknown` predicate
  behavior.

## Related

- #2360 — parent issue, this is stage 2 of two (originally; stage 3
  for `AttributeParameter` is now also implied).
- #2361 / PR #2362 — stage 1, implemented inference and wired it into
  the three surfaces; this stage drops the `Option`.
- #2358 / PR #2359 — closed the annotated-downcast direction; with
  stage 2 the unannotated direction is structurally impossible.
- #2357 — recursive inference through `upstream_state`, deferred from
  stage 1; tracked separately.
