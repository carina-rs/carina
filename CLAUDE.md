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
cargo test -p carina-cli

# Run a single test
cargo test -p carina-core test_name

# Run CLI commands (path must be a directory, not a file)
cargo run -- validate .
cargo run -- plan .
cargo run -- apply .

# With AWS credentials (using aws-vault)
aws-vault exec <profile> -- cargo run -- plan .
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
- Use full workspace `cargo build` / `cargo test` only when changes affect multiple crates or before creating a PR
- For `cargo check`, prefer `cargo check -p <crate-name>` as well
- Provider crates (aws, awscc) are in separate repositories — changes here may require updating those repos

### Build Cache Setup (sccache + shared target)

To speed up builds across git worktrees, set up sccache and a shared target directory. Without this, each worktree recompiles all dependencies from scratch.

```bash
# Install sccache
brew install sccache

# Create .cargo/config.toml (gitignored, local only)
mkdir -p .cargo
cat > .cargo/config.toml << 'EOF'
[build]
rustc-wrapper = "sccache"
target-dir = "/Users/mizzy/.cargo-target/carina"
EOF
```

This configuration:
- **sccache**: Caches compiled artifacts globally. New worktrees hit the cache instead of recompiling.
- **shared target-dir**: All worktrees share one target directory. Cargo uses file locks to handle concurrent access (builds are serialized, not parallel).

Note: `.cargo/config.toml` is gitignored because it contains machine-specific paths. Each new worktree needs this file copied or recreated.

## Architecture

Carina is a functional infrastructure management tool that treats side effects as values (Effects) rather than immediately executing them.

### Data Flow

```
DSL (.crn) → Parser → Resources → Differ → Plan (Effects) → Provider → Infrastructure
```

### Key Abstractions

- **Effect** (`carina-core/src/effect.rs`): Enum representing side effects (Create, Update, Delete, Read). Effects are values, not executed operations.
- **Plan** (`carina-core/src/plan.rs`): Collection of Effects. Immutable, can be inspected before execution.
- **Provider** trait (`carina-core/src/provider.rs`): Async trait for infrastructure operations. Returns `BoxFuture` for async methods.

### Repository Structure (Polyrepo)

Carina is split across multiple repositories under [carina-rs](https://github.com/carina-rs):

| Repository | Description |
|------------|-------------|
| **carina** (this repo) | Core, CLI, LSP, plugin SDK/host, state, TUI |
| [carina-provider-aws](https://github.com/carina-rs/carina-provider-aws) | AWS provider (Smithy-based codegen) |
| [carina-provider-awscc](https://github.com/carina-rs/carina-provider-awscc) | AWS Cloud Control provider |

Provider repositories depend on this repo's crates via `git` dependencies.

### Crate Structure (this repo)

- **carina-core**: Core library with parser, types, and traits. No AWS dependencies.
- **carina-cli**: Binary that wires everything together.
- **carina-aws-types**: AWS-specific type definitions shared by providers.
- **carina-plugin-host**: WASM plugin host for loading provider plugins.
- **carina-plugin-sdk**: SDK for building WASM provider plugins.
- **carina-provider-mock**: Mock provider for testing.
- **carina-provider-protocol**: Protocol definitions for provider communication.
- **carina-state**: State management.
- **carina-lsp**: Language Server Protocol implementation.
- **carina-tui**: Terminal UI for plan display.

### DSL Parser

The parser uses [pest](https://pest.rs/) grammar defined in `carina-core/src/parser/carina.pest`. Key constructs:
- `provider <name> { ... }` - Provider configuration
- `<provider>.<service>.<resource_type> { ... }` - Anonymous resource (ID from `name` attribute)
- `let <binding> = <resource>` - Named resource binding

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

### Resource Type Mapping

Resource types in DSL use dot notation (`s3.bucket`, `ec2.vpc`). When mapping between DSL resource types and schema lookups:
- DSL: `aws.s3.bucket` → Schema key: `s3.bucket`
- Ensure `extract_resource_type()` in `completion/mod.rs` and resource type validation in `diagnostics/mod.rs` use consistent dot notation

### Validation Formats

- **Region**: Accepts both DSL format (`aws.Region.ap_northeast_1`) and AWS string format (`"ap-northeast-1"`). Validation normalizes both to AWS format for comparison.
- **S3 Versioning**: Uses enum `Enabled`/`Suspended`, not boolean. AWS SDK returns these exact strings.

### Namespaced Enum Identifiers

Enum values use namespaced identifiers like `aws.s3.VersioningStatus.Enabled`.

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

### Submodule Initialization

This repo uses a git submodule for `carina-plugin-wit/`. After `git pull` or creating a new worktree, initialize the submodule:

```bash
git submodule update --init --recursive
```

Without this, builds will fail because `wit_bindgen::generate!` cannot find the WIT files.

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
