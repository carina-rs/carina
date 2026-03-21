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

### Plan Display Testing

When modifying plan display code (`display.rs`, `carina-tui`), use fixture-based testing:

```bash
# Visual confirmation with fixture data (no AWS needed)
make plan-all-create      # All resources new (Create only)
make plan-mixed           # Mixed: Create + Update + Delete
make plan-delete          # Orphan resource deletion
make plan-compact         # Compact mode
make plan-mixed-tui       # TUI mode
make plan-fixtures        # Run all patterns

# Snapshot tests (automated, runs in CI)
cargo test -p carina-cli plan_snapshot
```

Fixture files are in `carina-cli/tests/fixtures/plan_display/`. Each directory contains a `.crn` file and optionally a `carina.state.json` (state v3 with binding/dependency_bindings). When adding new plan display features, add a fixture and snapshot test to cover the new behavior.

When plan output changes (intentionally), update snapshots:

```bash
# Review and accept snapshot changes interactively
cargo insta review

# Or accept all pending snapshots
cargo insta accept
```

If snapshots are not updated after a display change, CI will fail on the `Test` job.

### Incremental Build Strategy

When working on a specific crate, always use crate-specific commands to avoid unnecessary compilation:

```bash
# Prefer crate-specific builds over full workspace builds
cargo build -p carina-core          # Instead of `cargo build`
cargo test -p carina-core           # Instead of `cargo test`

# Only use full workspace build/test when changes span multiple crates
cargo build
cargo test
```

Key rules:
- After modifying a single crate, build/test only that crate with `-p <crate-name>`
- After modifying codegen, build only the affected provider crate (e.g., `cargo build -p carina-provider-awscc`)
- Use full workspace `cargo build` / `cargo test` only when changes affect multiple crates or before creating a PR
- For `cargo check`, prefer `cargo check -p <crate-name>` as well

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
- `<provider>.<service>.<resource_type> { ... }` - Anonymous resource (ID from `name` attribute)
- `let <binding> = <resource>` - Named resource binding

### Region Format Conversion

The DSL uses `aws.Region.ap_northeast_1` format, but AWS SDK uses `ap-northeast-1`. Conversion happens in:
- `carina-provider-aws/src/lib.rs`: `convert_region_value()` for DSL→SDK
- Provider read operations return DSL format for consistent state comparison

### LSP Integration

When modifying the DSL or resource schemas, also update the LSP:

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

**Testing**: When bugs are found or issues are pointed out, write test code to capture the fix. This ensures regressions are caught and documents expected behavior.

### Provider-Specific Types

AWS-specific type definitions (e.g., region validation, versioning status) belong in `carina-provider-aws/src/schemas/types.rs` and `carina-provider-awscc/src/schemas/generated/mod.rs`, NOT in `carina-core`. Keep `carina-core` provider-agnostic.

### Resource Type Mapping

Resource types in DSL use dot notation (`s3.bucket`, `ec2.vpc`). When mapping between DSL resource types and schema lookups:
- DSL: `aws.s3.bucket` → Schema key: `s3.bucket`
- Ensure `extract_resource_type()` in `completion/mod.rs` and resource type validation in `diagnostics/mod.rs` use consistent dot notation

### Validation Formats

- **Region**: Accepts both DSL format (`aws.Region.ap_northeast_1`) and AWS string format (`"ap-northeast-1"`). Validation normalizes both to AWS format for comparison.
- **S3 Versioning**: Uses enum `Enabled`/`Suspended`, not boolean. AWS SDK returns these exact strings.

### Namespaced Enum Identifiers

Enum values use namespaced identifiers like `aws.s3.VersioningStatus.Enabled` or `awscc.ec2.vpc.InstanceTenancy.default`.

**When adding new namespaced patterns:**

1. **Pattern matching must handle digits** - Resource names like `s3` contain digits. Use `c.is_ascii_digit()` in addition to `c.is_lowercase()`:
   ```rust
   // Wrong: "s3" fails because '3'.is_lowercase() == false
   resource.chars().all(|c| c.is_lowercase() || c == '_')

   // Correct:
   resource.chars().all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_')
   ```

2. **Update `is_dsl_enum_format()` in `carina-core/src/utils.rs`** for new patterns (e.g., 4-part identifiers like `provider.resource.TypeName.value`)

3. **Plan display should not quote namespaced identifiers** - They are identifiers, not strings

4. **LSP diagnostics must validate Custom types** - When adding `AttributeType::Custom` with a validate function, ensure `carina-lsp/src/diagnostics/mod.rs` calls the validate function for editor warnings

5. **Always test with actual values** - Don't assume pattern matching works; write a quick test to verify

### Struct Types

`AttributeType::Struct` represents nested objects with typed fields.

**Key points:**
- Defined in `carina-core/src/schema.rs` as `Struct { name, fields: Vec<StructField> }`
- Each `StructField` has: `name`, `field_type` (recursive AttributeType), `required`, `description`
- AWSCC provider converts between DSL snake_case and CloudFormation PascalCase (see `carina-provider-awscc/src/provider.rs`)
- Codegen resolves CloudFormation `$ref` and inline object definitions into Struct types

**LSP integration:**
- When adding Struct validation, update `carina-lsp/src/diagnostics/validation.rs` to validate nested fields
- Completion should work recursively for struct fields

### Module Loading

Directory-based modules (e.g., `modules/web_tier/`) require special handling:
- CLI: `load_module()` checks `is_dir()` and reads `main.crn` from directory
- LSP: Module loading in `diagnostics/checks.rs` handles directory modules for proper validation

## Git Workflow

### Worktree-Based Development

**IMPORTANT: Use `git wt` (NOT `git worktree`).** `git wt` is a separate tool (`/opt/homebrew/bin/git-wt`) with its own syntax. NEVER use `git worktree` commands.

```bash
# Create worktree for a new task
git wt <branch-name> main

# List worktrees
git wt

# Delete worktree after PR is merged (from main worktree, not feature worktree)
git wt -d <branch-name>
```

### cfn-schema-cache Copy for Worktrees

`carina-provider-awscc/cfn-schema-cache/` is in `.gitignore` and NOT copied to worktrees. You MUST copy it manually using **absolute paths** after creating a worktree. Relative paths cause "are identical (not copied)" errors.

```bash
MAIN_REPO=/Users/mizzy/src/github.com/carina-rs/carina
WORKTREE=$MAIN_REPO/.worktrees/<branch>
mkdir -p $WORKTREE/carina-provider-awscc/cfn-schema-cache
cp -r $MAIN_REPO/carina-provider-awscc/cfn-schema-cache/* $WORKTREE/carina-provider-awscc/cfn-schema-cache/
```

**Source MUST be the main repo absolute path. NEVER use relative paths.**

### Branch Cleanup

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
