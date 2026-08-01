---
name: lsp-integration
description: Which carina-lsp sites to update when changing the Carina DSL or resource schemas — completion, semantic tokens, diagnostics, and resource type mapping. Use when adding DSL constructs, attributes, enum values, or resource types.
---

# LSP Integration

When modifying the DSL or resource schemas, also update the LSP. Validate
and LSP must stay at parity: a rule enforced by `carina validate` but not
surfaced in the editor (or vice versa) is a bug.

- **Completion** (`carina-lsp/src/completion/`):
  - `top_level_completions()` in `top_level.rs`: Add keywords (e.g., `backend`, `provider`, `let`)
  - `attribute_completions_for_type()` in `values.rs`: Add attribute completions for resource types
  - `value_completions_for_attr()` in `values.rs`: Add value completions for specific attributes

- **Semantic Tokens** (`carina-lsp/src/semantic_tokens.rs`):
  - `tokenize_line()`: Add keyword highlighting for new DSL constructs
  - Keywords like `provider`, `backend`, `let` are highlighted at line start

- **Diagnostics** (`carina-lsp/src/diagnostics/`):
  - `mod.rs`: Core diagnostic logic and type validation
  - `validation.rs`: Struct and nested field validation
  - `checks.rs`: Module loading and additional checks
  - Parser errors are automatically detected via `carina-core::parser`

- **Custom types**: when adding a custom type (built via
  `AttributeType::custom(...)`) with a validate function, ensure
  `carina-lsp/src/diagnostics/mod.rs` calls that validate function so the
  warning reaches the editor.

- **Struct types**: when adding Struct validation, update
  `carina-lsp/src/diagnostics/validation.rs` to validate nested fields;
  completion should work recursively for struct fields.

## Resource Type Mapping

Resource types in DSL use dot notation (`s3.bucket`, `ec2.vpc`). When
mapping between DSL resource types and schema lookups:

- DSL: `aws.s3.Bucket` → Schema key: `s3.bucket`
- Ensure `extract_resource_type()` in `completion/mod.rs` and resource type
  validation in `diagnostics/mod.rs` use consistent dot notation.

## Directory-scoped, always

LSP features read DSL source, so the directory-scoped rule in `CLAUDE.md`
applies in full: a `let` binding in `main.crn` must be visible from
`exports.crn`. Any handler that looks at only the current buffer is a bug —
acceptance tests must use a multi-file `tempfile::tempdir()` fixture, not a
single source string.
