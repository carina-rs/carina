# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Test Commands

```bash
# Build
cargo build

# Run all tests
cargo test

# Run tests for a specific crate
cargo test -p carina-core
cargo test -p carina-provider-aws

# Run a single test
cargo test -p carina-core test_name

# Run CLI commands
cargo run -- validate example.crn
cargo run -- plan example.crn
cargo run -- apply example.crn

# With AWS credentials (using aws-vault)
aws-vault exec <profile> -- cargo run -- plan example.crn
```

## Architecture

Carina is a functional infrastructure management tool that treats side effects as values (Effects) rather than immediately executing them.

### Data Flow

```
DSL (.crn) → Parser → Resources → Differ → Plan (Effects) → Interpreter → Provider → Infrastructure
```

### Key Abstractions

- **Effect** (`carina-core/src/effect.rs`): Enum representing side effects (Create, Update, Delete, Read). Effects are values, not executed operations.
- **Plan** (`carina-core/src/plan.rs`): Collection of Effects. Immutable, can be inspected before execution.
- **Provider** trait (`carina-core/src/provider.rs`): Async trait for infrastructure operations. Returns `BoxFuture` for async methods.
- **Interpreter** (`carina-core/src/interpreter.rs`): Executes a Plan by dispatching Effects to a Provider.

### Crate Structure

- **carina-core**: Core library with parser, types, and traits. No AWS dependencies.
- **carina-provider-aws**: AWS implementation of Provider trait using `aws-sdk-s3`.
- **carina-cli**: Binary that wires everything together.

### DSL Parser

The parser uses [pest](https://pest.rs/) grammar defined in `carina-core/src/parser/carina.pest`. Key constructs:
- `provider <name> { ... }` - Provider configuration
- `<provider>.<service>.<resource> { ... }` - Anonymous resource (ID from `name` attribute)
- `let <binding> = <resource>` - Named resource binding

### Region Format Conversion

The DSL uses `aws.Region.ap_northeast_1` format, but AWS SDK uses `ap-northeast-1`. Conversion happens in:
- `carina-provider-aws/src/lib.rs`: `convert_region_value()` for DSL→SDK
- Provider read operations return DSL format for consistent state comparison

### LSP Integration

When modifying the DSL or resource schemas, also update the LSP:

- **Completion** (`carina-lsp/src/completion.rs`):
  - `top_level_completions()`: Add keywords (e.g., `backend`, `provider`, `let`)
  - `attribute_completions_for_type()`: Add attribute completions for resource types
  - `value_completions_for_attr()`: Add value completions for specific attributes

- **Semantic Tokens** (`carina-lsp/src/semantic_tokens.rs`):
  - `tokenize_line()`: Add keyword highlighting for new DSL constructs
  - Keywords like `provider`, `backend`, `let` are highlighted at line start

- **Diagnostics** (`carina-lsp/src/diagnostics.rs`):
  - Add type validation for new types
  - Parser errors are automatically detected via `carina-core::parser`

**Testing**: When bugs are found or issues are pointed out, write test code to capture the fix. This ensures regressions are caught and documents expected behavior.

### Provider-Specific Types

AWS-specific type definitions (e.g., region validation, versioning status) belong in `carina-provider-aws/src/schemas/types.rs` and `carina-provider-awscc/src/schemas/generated/mod.rs`, NOT in `carina-core`. Keep `carina-core` provider-agnostic.

### Resource Type Mapping

Resource types in schemas use underscore format (`s3_bucket`, `ec2_vpc`). When mapping between DSL resource types and schema lookups:
- DSL: `aws.s3_bucket` → Schema key: `s3_bucket`
- Ensure `extract_resource_type()` in completion.rs and `valid_resource_types` in diagnostics.rs use consistent underscore notation

### Validation Formats

- **Region**: Accepts both DSL format (`aws.Region.ap_northeast_1`) and AWS string format (`"ap-northeast-1"`). Validation normalizes both to AWS format for comparison.
- **S3 Versioning**: Uses enum `Enabled`/`Suspended`, not boolean. AWS SDK returns these exact strings.

### Namespaced Enum Identifiers

Enum values use namespaced identifiers like `aws.s3.VersioningStatus.Enabled` or `awscc.ec2_vpc.InstanceTenancy.default`.

**When adding new namespaced patterns:**

1. **Pattern matching must handle digits** - Resource names like `s3` contain digits. Use `c.is_ascii_digit()` in addition to `c.is_lowercase()`:
   ```rust
   // Wrong: "s3" fails because '3'.is_lowercase() == false
   resource.chars().all(|c| c.is_lowercase() || c == '_')

   // Correct:
   resource.chars().all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_')
   ```

2. **Update `is_dsl_enum_format()` in `carina-cli/src/main.rs`** for new patterns (e.g., 4-part identifiers like `provider.resource.TypeName.value`)

3. **Plan display should not quote namespaced identifiers** - They are identifiers, not strings

4. **LSP diagnostics must validate Custom types** - When adding `AttributeType::Custom` with a validate function, ensure `carina-lsp/src/diagnostics.rs` calls the validate function for editor warnings

5. **Always test with actual values** - Don't assume pattern matching works; write a quick test to verify

### Struct Types

`AttributeType::Struct` represents nested objects with typed fields.

**Key points:**
- Defined in `carina-core/src/schema.rs` as `Struct { name, fields: Vec<StructField> }`
- Each `StructField` has: `name`, `field_type` (recursive AttributeType), `required`, `description`
- AWSCC provider converts between DSL snake_case and CloudFormation PascalCase (see `carina-provider-awscc/src/provider.rs`)
- Codegen resolves CloudFormation `$ref` and inline object definitions into Struct types

**LSP integration:**
- When adding Struct validation, update `carina-lsp/src/diagnostics.rs` to validate nested fields
- Completion should work recursively for struct fields

### Module Loading

Directory-based modules (e.g., `modules/web_tier/`) require special handling:
- CLI: `load_module()` checks `is_dir()` and reads `main.crn` from directory
- LSP: `load_directory_module()` in diagnostics.rs handles directory modules for proper validation

## Git Workflow

After merging a PR, clean up branches:
```bash
git checkout main
git pull
git branch -d <feature-branch>    # Delete local branch
git remote prune origin           # Remove stale remote tracking branches
```

## Code Style

- **Commit messages**: Write in English
- **Code comments**: Write in English
