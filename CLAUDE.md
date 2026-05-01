# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Test Commands

```bash
# Compile-only sanity check (faster than `cargo build`; does not link binaries)
cargo check

# Run all tests with nextest (preferred; 2-3x faster than `cargo test`)
cargo nextest run

# Run tests for a specific crate
cargo nextest run -p carina-core
cargo nextest run -p carina-cli

# Run a single test
cargo nextest run -p carina-core test_name

# Doctests (nextest does not run them — cover them with cargo test --doc
# before opening a PR)
cargo test --workspace --doc

# Run CLI commands (path must be a directory, not a file)
cargo run -- validate .
cargo run -- plan .
cargo run -- apply .

# With AWS credentials (using aws-vault)
aws-vault exec <profile> -- cargo run -- plan .
```

Install nextest once: `cargo install cargo-nextest --locked`. Plain
`cargo test` still works and produces identical results, but nextest
is the recommended runner for the verify cycle — it parallelizes
test processes and reports failures faster.

### Verify Protocol — Do Not Run Redundant Builds

The verify cycle is the slowest thing about working on this repo. Most of
that cost is cargo work, and the single biggest waste is running
`cargo build` immediately before `cargo test`: the test step compiles
the same artifacts the build step just produced, doubling the wait.

**Rules:**

- **Do not run `cargo build`** as a separate verification step.
  `cargo nextest run` (or `cargo test`) already compiles everything it
  needs. Running both is pure duplication.
- For a faster compile-only sanity check during iteration, use
  `cargo check -p <crate>` (skips linking, ~30–50% faster than `cargo build`).
- The only legitimate use of `cargo build` in the verify cycle is
  `cargo build --release` immediately before opening a PR, to catch
  release-only issues that debug builds miss. Skip it for refactors,
  bug fixes, or anything that has not changed `Cargo.toml` / unsafe code /
  the `release` profile config.
- Order your verify cycle as: `cargo nextest run -p <crate>` → broaden to
  `cargo nextest run --workspace` only when the change spans crates →
  `cargo test --workspace --doc` (nextest skips doctests) →
  `cargo clippy --workspace --all-targets -- -D warnings` →
  `bash scripts/check-*.sh`.
- The `cargo test --doc` step is cheap once `cargo nextest run` has
  already compiled the crates, so always run it before declaring verify
  done — public API doctests in `carina-core/src/utils.rs` and elsewhere
  are not covered by nextest.

CI's `Test` job runs `cargo build -p carina-provider-mock --target wasm32-wasip2`
*before* the test step, but that build targets a different platform
(`wasm32-wasip2`) than the test step (host), so it is not redundant —
it produces the WASM fixture that `carina-plugin-host`'s integration
tests load. Do not generalize from that step to local development.

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
cargo nextest run -p carina-cli plan_snapshot
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
# Prefer crate-scoped check/test over full workspace runs
cargo check -p carina-core               # Fastest sanity check while iterating
cargo nextest run -p carina-core         # Compiles + runs the crate's tests

# Only use full workspace test when changes span multiple crates,
# or as the final pre-PR sweep
cargo nextest run
cargo test --workspace --doc             # Add this to cover doctests
```

Key rules:
- After modifying a single crate, test only that crate with `cargo nextest run -p <crate-name>`. Do **not** run `cargo build -p <crate-name>` first — the test command does the build.
- Use full workspace `cargo nextest run` only when changes affect multiple crates or before creating a PR; remember to follow it with `cargo test --workspace --doc` for doctests.
- For the fastest iteration loop, `cargo check -p <crate-name>` skips linking and is ~30–50% faster than `cargo build -p <crate-name>`.
- Provider crates (aws, awscc) are in separate repositories — changes here may require updating those repos.

### Build Cache Setup (sccache, per-worktree target)

To speed up builds across git worktrees, set up sccache. Each worktree
keeps its own `target/` directory; sccache provides cross-worktree reuse
at the rustc-invocation level.

```bash
# Install sccache
brew install sccache

# Create .cargo/config.toml (gitignored, local only)
mkdir -p .cargo
cat > .cargo/config.toml << 'EOF'
[build]
rustc-wrapper = "sccache"
EOF
```

Why this shape:

- **sccache** caches compiled artifacts by content hash globally. New
  worktrees hit the cache at the rustc-call level instead of recompiling
  dependencies from scratch — that is where the cross-worktree reuse
  actually comes from.
- **Per-worktree `target/`** (the cargo default — no `target-dir` override)
  keeps each worktree's incremental build state local. Cargo locks the
  target directory while building, so a single shared `target-dir` across
  worktrees serializes parallel work and produces "Blocking waiting for
  file lock on artifact directory" stalls when multiple agents run at
  once. Per-worktree `target/` removes that contention; sccache still
  carries the dependency-level reuse.

Earlier guidance recommended `target-dir = "/Users/mizzy/.cargo-target/carina"`
in `.cargo/config.toml`. That is now discouraged. If you have the override
locally, drop it:

```bash
# Edit .cargo/config.toml and remove the `target-dir = ...` line
```

This is currently a pilot — the per-worktree shape is the new default,
but real wall-clock numbers will be collected over the next few PR cycles
before the change is considered final (#2290).

Note: `.cargo/config.toml` is gitignored because it contains
machine-specific paths. Each new worktree needs the file copied or
recreated. The `target/` directory inside the worktree should also be
gitignored (it already is at the workspace level).

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
- **carina-plugin-host**: WASM plugin host for loading provider plugins.
- **carina-plugin-sdk**: SDK for building WASM provider plugins.
- **carina-provider-mock**: Mock provider for testing.
- **carina-provider-protocol**: Protocol definitions for provider communication.
- **carina-provider-resolver**: Resolves and loads provider plugins for the CLI.
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

- **TextMate grammars** (editor syntax highlighting):
  - `editors/vscode/syntaxes/carina.tmLanguage.json` and
    `editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json` must stay
    **byte-identical**. Edit both files together; the parity test in
    `carina-core/tests/tmlanguage_keyword_parity.rs` fails the build
    otherwise.

**Testing**: When bugs are found or issues are pointed out, write test code to capture the fix. This ensures regressions are caught and documents expected behavior.

### Resource Type Mapping

Resource types in DSL use dot notation (`s3.bucket`, `ec2.vpc`). When mapping between DSL resource types and schema lookups:
- DSL: `aws.s3.Bucket` → Schema key: `s3.bucket`
- Ensure `extract_resource_type()` in `completion/mod.rs` and resource type validation in `diagnostics/mod.rs` use consistent dot notation

### Validation Formats

- **Region**: Accepts both DSL format (`aws.Region.ap_northeast_1`) and AWS string format (`"ap-northeast-1"`). Validation normalizes both to AWS format for comparison.
- **S3 Versioning**: Uses enum `aws.s3.VersioningStatus.Enabled` / `aws.s3.VersioningStatus.Suspended` in DSL (PascalCase matches the AWS SDK representation).

### Namespaced Enum Identifiers

Enum values use namespaced identifiers like `aws.s3.Bucket.VersioningStatus.enabled`.

**When adding new namespaced patterns:**

1. **Pattern matching must handle digits** - Resource names like `s3` contain digits. Use `c.is_ascii_digit()` in addition to `c.is_lowercase()`:
   ```rust
   // Wrong: "s3" fails because '3'.is_lowercase() == false
   resource.chars().all(|c| c.is_lowercase() || c == '_')

   // Correct:
   resource.chars().all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_')
   ```

2. **Update `NamespacedId::parse` in `carina-core/src/utils.rs`** for new
   patterns. It is the single source of truth for the 2/3/4/5-part shapes
   and the four sibling utilities (`is_dsl_enum_format`,
   `convert_enum_value`, `extract_enum_value_with_values`,
   `validate_enum_namespace`) all delegate to it. **TypeName is pinned at
   index 3 for 5+ part inputs** so PascalCase resource segments (`Vpc`)
   parse correctly and dotted values (`ipsec.1`) flow into the trailing
   slice — preserve this invariant when adding shapes.

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

Modules are directory-scoped. An import target must be a directory containing
one or more `.crn` files; single-file modules are rejected with
`ModuleError::NotADirectory`. Inside a module directory all `.crn` files are
merged uniformly — no file name (including `main.crn`) is privileged.

- CLI: `load_module()` / `ModuleResolver::load_module` require a directory path.
- LSP: Module loading in `diagnostics/checks.rs` handles directory modules for proper validation.

### Directory-scoped, never single-file

**Carina configurations are directory units.** Every feature that reads DSL
source — completion, diagnostics, validation, formatting, hover, code lens,
etc. — must consider sibling `.crn` files in the same directory as
first-class input, not as an afterthought. A `let` binding in `main.crn`
must be visible from `exports.crn`; a `provider` block in `providers.crn`
must apply to `main.crn`. Anything that looks at only one file is a bug.

When implementing or modifying such a feature:

1. **Acceptance test must use multiple files.** A unit test built from a
   single string is not sufficient evidence. Write a `tempfile::tempdir()`
   fixture that mirrors the real `infra/aws/management/<dir>/` shape —
   typically `main.crn` + `exports.crn` + `providers.crn` + `backend.crn` —
   and assert behavior under that shape. The bare-string variant is fine
   *in addition to* the directory variant, never *instead of* it.
2. **Real-infra smoke test.** Where feasible, run the built binary
   (`carina fmt`, `carina validate`, etc.) against `carina-rs/infra/`
   directly before declaring the issue done. The acceptance condition
   in the original issue almost always names a real path; respect it.
3. **API signal.** Helpers that take a single `&str` of source text
   (`extract_resource_bindings`, `extract_let_bindings`, similar
   text-scan utilities) silently invite single-file thinking. When you
   see one in the call path, the question to ask is "does this caller
   need the sibling files too?" — most of the time the answer is yes.

Past breakage from violating this rule: PR #2120 (closed #2043 too
early) shipped `exports` value-position completion that scanned only
the current buffer; the real `exports.crn` references bindings in
`main.crn`, so users got zero LSP candidates. PR #2121 fixed it by
threading `base_path` into the same handler. Same class of mistake
recurred from PR #2118 (closed #2117 prematurely) where the formatter
fix passed unit tests but never ran against the real infra fixture.

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

<!-- dagayn MCP tools -->
## MCP Tools: dagayn

**IMPORTANT: This project has a knowledge graph. ALWAYS use the
dagayn MCP tools BEFORE using Grep/Glob/Read to explore
the codebase.** The graph is faster, cheaper (fewer tokens), and gives
you structural context (callers, dependents, test coverage) that file
scanning cannot.

### When to use graph tools FIRST

- **Exploring code**: `semantic_search_nodes` or `query_graph` instead of Grep
- **Understanding impact**: `get_impact_radius` instead of manually tracing imports
- **Code review**: `detect_changes` + `get_review_context` instead of reading entire files
- **Finding relationships**: `query_graph` with callers_of/callees_of/imports_of/tests_for
- **Architecture questions**: `get_architecture_overview` + `list_communities`

Fall back to Grep/Glob/Read **only** when the graph doesn't cover what you need.

### Key Tools

| Tool | Use when |
| ------ | ---------- |
| `detect_changes` | Reviewing code changes — gives risk-scored analysis |
| `get_review_context` | Need source snippets for review — token-efficient |
| `get_impact_radius` | Understanding blast radius of a change |
| `get_affected_flows` | Finding which execution paths are impacted |
| `query_graph` | Tracing callers, callees, imports, tests, dependencies |
| `semantic_search_nodes` | Finding functions/classes by name or keyword |
| `get_architecture_overview` | Understanding high-level codebase structure |
| `refactor_tool` | Planning renames, finding dead code |

### Workflow

1. The graph auto-updates on file changes (via hooks).
2. Use `detect_changes` for code review.
3. Use `get_affected_flows` to understand impact.
4. Use `query_graph` pattern="tests_for" to check coverage.

<!-- dagayn markdown policy -->
## Markdown documentation policy: declare dependencies via directive comments

When authoring or editing a Markdown document in this repository, declare
inter-section and inter-document dependencies as HTML directive comments so
they are captured by the dagayn graph (`DEPENDS_ON` / `IMPORTS_FROM` edges)
and discoverable via `query_graph` / `get_impact_radius`.

### Required form

```markdown
<!-- <kind> <target> -->
```

`<kind>` MUST be one of: `constrained-by`, `blocked-by`, `supersedes`,
`derived-from`. Choose the kind whose semantics best match the dependency:

| Kind | Use when |
| ---- | -------- |
| `constrained-by` | This section's design is bounded by the referenced document/section |
| `blocked-by` | This item cannot proceed until the referenced item resolves |
| `supersedes` | This document replaces the referenced content |
| `derived-from` | This section is derived from the referenced source |

### Three target shapes

| Dependency type | Target syntax | Example |
| --------------- | ------------- | ------- |
| Within-document section | `#section-slug` | `<!-- derived-from #background -->` |
| Other document (whole file) | `./relative/path.md` | `<!-- blocked-by ./specs/open-issue.md -->` |
| Other document + section | `./path.md#slug` | `<!-- constrained-by ./adr.md#context -->` |

Slugs follow GitHub Markdown rules: lowercase, non-alphanumerics removed,
spaces and hyphens collapsed to `-`. Place the directive immediately under
the heading whose content depends on the target. External URLs
(`http://`, `https://`) are not graph-resolvable — keep them as ordinary
Markdown links, not directive targets.

### When to add a directive

- Section design references an ADR, spec, or research note → `constrained-by` or `derived-from`.
- A document replaces an older one → `supersedes` (place in the new document).
- A spec/task section is blocked on another being resolved → `blocked-by`.
- A later section extends an earlier one non-obviously → `derived-from #earlier-section`.

If no real dependency exists, do not invent one. Directives are signal, not decoration.
