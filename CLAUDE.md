# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Root-cause fixes only — no bandaids, no per-symptom carve-outs

**The most important rule of this project.** When fixing a bug, fix the root cause, not the symptom. If the same broken invariant produces symptoms in multiple code paths (`apply`, `destroy`, `state refresh`, `plan`, etc.), the correct fix is the *one* upstream change that restores the invariant, not a filter / guard / carve-out at every consumer site.

- **Never propose "minimal fix in this PR, follow-up issue for the rest"** when "the rest" is the same class of bug at sibling call sites. That is a bandaid presented as scope discipline. The correct framing is: this is one bug, fix the root.
- **Never invoke "1 PR = 1 topic" to justify a per-site patch.** "1 topic" means one *root cause*, not "one of several symptoms of the same root cause." Fixing the root *is* the topic. Concretely: if the bug is "data sources should not live in `state.resources`", the fix is "prune them at state read", not "filter them at every consumer of the overlay." If the bug is "`Effect` arms diverge on field X", the fix is at the enum / type level, not at every match site.
- **Self-check before opening a PR:** if the diff filters / guards / skips for the buggy condition instead of removing the condition itself, the fix is symptom-level. Step back and find the upstream seam.
- **5-round review passing is NOT evidence the fix is root-cause.** A bandaid can pass every gate. Ask "if a new caller appears tomorrow, does it need to remember this filter too?" — if yes, the root is still broken.
- **When in doubt, pick the broader fix.** Past failure mode in this repo: shrinking scope and offering a follow-up has been pushed back on every single time. The user has explicitly said: do not present "fix here + follow-up for sibling" — fix the root once.
- **Type Safety First applies here too:** prefer the fix that makes the broken state unrepresentable. Newtypes, tagged unions, typestate — over runtime filters at every consumer.

## Type Safety First

- When fixing bugs, prefer type-level solutions (newtypes, tagged unions, typestate) over runtime validation/filters.
- Type safety is part of the same 'topic' as the bug fix - do not defer it to a separate PR.
- For enums/variants that carry different data, use tagged unions rather than optional fields with runtime checks.

## Long-term view alongside root-cause

"Root cause" answers *what is broken right now*; "long-term view + type
safety" answers *will the same class of bug be reachable again by a
future caller*. Both questions must be answered before declaring a fix
complete — passing the first is not evidence of passing the second.

- **Both lenses, every fix.** When proposing a fix, evaluate it under
  both lenses: (1) does it restore the invariant at the upstream seam
  (root cause)? (2) does the type system make the broken state
  unrepresentable for any future caller (long-term)? A fix that
  answers yes-to-(1) but no-to-(2) is **a runtime patch at multiple
  consumer sites disguised as a root-cause fix** — it works today and
  silently regresses when the next consumer is added.
- **The "new caller tomorrow" check is type-shaped, not behavioral.**
  Asking "if a new caller appears tomorrow, does it need to remember
  this filter too?" is the right question — but the answer must come
  from the *type signature*, not from documentation or convention. If
  the answer is "the caller has to remember to call `find_*` /
  `resolve_*` / `assert_*`", the root is still broken: the type
  permits the buggy path. Make the resolver step required by the type
  (return a wrapper type that only a resolver can produce; make the
  raw type uncomparable to the resolved type).
- **Measure radius before deferring.** The temptation to defer the
  type-level reshape to a follow-up issue is strongest when the
  runtime patch is in front of you and the typed reshape feels big.
  Always measure: stub the newtype, run
  `cargo check --workspace --all-targets 2>&1 | grep error | wc -l`,
  revert. A small number (single or low double digits) means **do it
  in-PR**, not as follow-up — the carina#3280 lesson (an explicit
  in-memory note in this repo's user-memory) is that "wide blast
  radius" intuitions have repeatedly been wrong.
- **When the radius is genuinely large**, file the type-level
  follow-up issue **in the same response** as the runtime fix PR —
  not "I might file it later". Reference the runtime PR and the
  remaining type hazard explicitly, so a future maintainer reading the
  PR can see why the runtime fix was chosen and what stays broken at
  the type level.
- **Self-check at PR creation:**
  - Does the diff add `find_*` / `resolve_*` / `lookup_*` calls at
    multiple consumer sites? If yes, ask whether a newtype could make
    the raw value impossible to use without resolution.
  - Does the fix rely on every consumer remembering to do something?
    If yes, the type system is the right place to enforce it.
  - Is there a sibling code path that does the same dance? If yes,
    the convention is leaking into multiple sites and the type is the
    factoring tool.

Past failure mode (carina#3324 / PR #3325 → carina#3326): a runtime
resolver fix at three consumer sites was reviewed across five rounds
and merged as "root-cause"; the user then asked whether the change was
type-safe and long-term. Honest answer: no — `ResourceId` still
permitted a routing-mismatch comparison, and a future fourth consumer
would re-introduce the same bug. The typed reshape
(`StateBlockAddress` newtype) became a follow-up issue rather than
landing in the same PR. The lesson is to apply *both* lenses at PR
creation, not after the user asks.

## Communication Style

- Be terse. Do not ask permission for obvious actions (e.g., using the correct AWS profile, standard build commands).
- When opening/merging a PR, always include the GitHub URL directly in the response.
- No sycophantic phrasing ('Great question!', 'You're absolutely right!').
- Do not over-explain hypothetical design options before checking the actual real-world case (screenshots, real output).

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

### Crate-Scoped Verify Helper

`scripts/touched-crates.sh` maps a set of changed files to the cargo
`-p <crate>` flags that exercise them, so iteration verify can stay
crate-scoped instead of always sweeping the workspace:

```bash
# What crates does the current branch touch (vs origin/main)?
scripts/touched-crates.sh
# → "-p carina-core -p carina-cli"   (or "--workspace", or empty)

# Run nextest only over the touched crates:
cargo nextest run $(scripts/touched-crates.sh)

# Diff against a specific base:
scripts/touched-crates.sh --base main

# Pipe a custom file list (e.g. only the dirty ones):
git diff --name-only | scripts/touched-crates.sh --stdin
```

Outputs and what they mean:

| Output           | Meaning                                                   |
| ---------------- | --------------------------------------------------------- |
| `-p <crate> ...` | Run only those crates' tests; transitive consumers are not invalidated by this change. |
| `--workspace`    | Cross-cutting change (touched `carina-core`, root `Cargo.toml`/`Cargo.lock`, or an unrecognized path). Sweep the whole workspace. |
| (empty)          | No test-relevant files changed (only docs / CI / scripts / infra). Skip the test step. |

The helper is intentionally pessimistic: when in doubt, it emits
`--workspace`. The two main "fall back" rules:

1. **`carina-core` touched ⇒ `--workspace`.** Every other crate
   depends on `carina-core`, so testing only `carina-core` would miss
   downstream regressions. Run the full sweep instead.
2. **Unknown path ⇒ `--workspace`.** A new top-level directory or a
   file in a path the helper does not classify is treated as
   workspace-wide until the helper learns about it.

For more rigorous transitive impact analysis (e.g. "which crates'
*tests* are affected when I change a function in `carina-core`?"),
use the `dagayn` MCP `get_impact_radius` tool, which walks the call
graph rather than relying on the directory mapping above.

The helper feeds the verify cycle order from #2289 / #2291:

```bash
# 1. Compile-only sanity (still crate-scoped during iteration)
cargo check -p <crate>

# 2. Tests, scoped to what changed
SCOPE=$(scripts/touched-crates.sh)
if [[ -n "$SCOPE" ]]; then
    cargo nextest run $SCOPE
fi

# 3. Doctests (cheap; nextest does not run them)
cargo test --workspace --doc

# 4. Lints
cargo clippy --workspace --all-targets -- -D warnings

# 5. Repo invariants
bash scripts/check-*.sh
```

Steps 3–5 stay workspace-scoped because they are already fast and
catch issues that a crate-scoped run would miss (cross-crate doctest
references, workspace-level lint config, repo-wide invariant scripts).

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

#### Cross-Worktree Caching with sccache-wrapper (recommended)

Plain sccache mixes the absolute source path into its cache key, so a
second worktree at the *same commit* still misses many entries — even
though the source is byte-identical. On a `carina-core` cold→warm
benchmark (build in worktree A, then the same commit in worktree B) the
second worktree took 7.6s with plain sccache because rustc still
recompiled most crates (`user` time stayed at ~10s).

[`sccache-wrapper`](https://github.com/moriyoshi/sccache-wrapper)
is a `RUSTC_WRAPPER` that normalizes the workspace root to a
`@@WORKSPACE@@` placeholder *before* computing the cache key, then
delegates to sccache. With it, worktree B of the same benchmark hit the
cache for all 52 crates and finished in 1.6s (`user` time ~1.3s) — a
~4.8x wall-clock win on the second-and-later worktree.

Setup:

```bash
# Build the wrapper from the sccache-wrapper repo. Unset RUSTC_WRAPPER first
# to avoid the wrapper recursively invoking itself during its own build.
git clone --depth 1 https://github.com/moriyoshi/sccache-wrapper.git /tmp/sccache-wrapper
( cd /tmp/sccache-wrapper && RUSTC_WRAPPER= cargo build --release )

# Install the binary somewhere on PATH (or note its absolute path).
mkdir -p ~/.local/bin
cp /tmp/sccache-wrapper/target/release/sccache-wrapper ~/.local/bin/

# Point .cargo/config.toml at the wrapper instead of sccache directly.
cat > .cargo/config.toml << 'EOF'
[build]
rustc-wrapper = "/Users/<you>/.local/bin/sccache-wrapper"

[env]
# Shared rustc cache — keep it OUTSIDE any worktree so every worktree
# reads and writes the same cache.
WB_RUSTC_CACHE_DIR = "/Users/<you>/.cache/sccache-wrapper-rustc-cache"
EOF
```

Notes and trade-offs:

- The wrapper *replaces* `rustc-wrapper = "sccache"` — it calls sccache
  internally, so do not chain both.
- Leave `WB_WORKSPACE_ROOT` unset. The wrapper then derives it per
  invocation via `git rev-parse --show-toplevel`, so the same
  `.cargo/config.toml` works in every worktree without per-worktree
  edits. Set it explicitly only to skip that subprocess overhead.
- The wrapper strips `-C incremental=…` (incremental compilation
  conflicts with deterministic output). For a tight edit-rebuild loop
  inside a *single* worktree, plain incremental builds can be faster;
  the wrapper's win is concentrated on the second-and-later worktree.
- A cold worktree sees little benefit (~8.4s vs ~9.6s in the
  benchmark). The payoff is cross-worktree reuse.
- `WB_RUSTC_CACHE_DEBUG=1` logs per-crate HIT/MISS to stderr;
  `sccache-wrapper --dump-cache` lists all cache entries.

Like the per-worktree `target/` change above, this is a pilot —
collect real wall-clock numbers over the next few PR cycles before
treating it as the established default (#2290).

### Multi-Worktree Parallel Verify

When 2+ worktrees are running `cargo nextest run` (or any
cargo build) at the same time, each worktree's verify cycle gets
noticeably slower. The contention has three sources, in rough order
of severity:

1. **sccache file-storage lock contention.** The default sccache
   backend serializes concurrent writers, so cross-worktree reuse
   stalls instead of accelerating.
2. **Duplicate dependency compilation.** Per-worktree `target/`
   removes cargo's "Blocking waiting for file lock" stalls but does
   not dedupe compile work. On a cache miss each worktree recompiles
   the same dependency graph independently.
3. **rustc / linker CPU + memory-bandwidth contention.** Each worktree
   spawns its own rustc and linker processes that compete for cores.
   Linking is especially memory-bandwidth heavy.

Mitigations, in the order to try them:

**1. Scope tests to touched crates.** This is the single biggest win
and applies even with one worktree. Use the
`scripts/touched-crates.sh` helper documented above instead of
defaulting to `--workspace`. A crate-local change reruns one crate's
tests; a `--workspace` run reruns every crate in the repo. Also
prefer `cargo check -p <crate>` for mid-iteration sanity and reserve
`cargo nextest run` for pre-PR.

**2. Use `cargo nextest run -j N` to cap test parallelism per
worktree when multiple worktrees are active.** Pick `N` so the total
across worktrees stays at or below the physical core count — e.g.
on a 16-core machine with 2 active worktrees, run each with `-j 8`.
Note this caps the *test execution* phase only, not the compile
phase: rustc/linker contention (source 3 above) is unaffected.
Repository default is left unchanged (no `.config/nextest.toml`)
because a fixed cap penalizes the common single-worktree case;
prefer the ad-hoc `-j` flag.

**3. Switch sccache to a Redis backend (opt-in).** Assumes sccache
is already wired in via the previous "Build Cache Setup" section.
Switching the backend removes the sccache file-storage lock
contention (source 1 above) and improves hit rate across worktrees,
which also reduces the duplicate compilation in source 2. This is
opt-in, not the repository default, because Redis is a long-running
service and the benefit only materializes when 2+ worktrees
regularly compile concurrently.

```bash
# Install and start Redis
brew install redis
brew services start redis

# Add to your shell rc (zsh, bash, etc.)
export SCCACHE_REDIS_ENDPOINT=redis://127.0.0.1:6379

# Restart sccache so the new backend takes effect
sccache --stop-server
sccache --start-server

# Watch hit rate (run again after a few builds to see ratio rise)
sccache --show-stats | grep -E "^Cache (hits|misses|hits rate)"
```

Cap Redis memory by setting `maxmemory` and
`maxmemory-policy allkeys-lru` in your Redis config
(`/opt/homebrew/etc/redis.conf` on Apple Silicon brew,
`/usr/local/etc/redis.conf` on Intel macOS brew).
`SCCACHE_CACHE_SIZE` only applies to the local file backend;
with Redis, the bound is set on the Redis side.

When the benefit fades (e.g. you stop using parallel worktrees) you
can revert by unsetting `SCCACHE_REDIS_ENDPOINT` — sccache falls
back to its default file storage automatically.

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

### Plan Concurrency Contract

<!-- constrained-by #key-abstractions -->

`carina plan` **takes no state lock.** `apply`/`destroy` acquire an
exclusive lock (`backend.acquire_lock(...)`); `plan` deliberately does
not. A lock on a read-only operation is overkill, and because the
backend lock API is exclusive-only it would serialize concurrent
`plan`s and let a long `plan` block deploys — too strong for a command
whose output is only a prediction. (This was a deliberate decision for
issue #3111; options considered were: (1) document as intentional,
(2) acquire a shared/read lock, (3) detect drift and warn. Option 3
was chosen — option 1 is weaker than Terraform and leaves the user
unaware of staleness; option 2 needs a shared-lock mode the backends
do not have and is only justified once a saved-plan/`plan -out`→`apply`
workflow exists.)

Because `plan` reads state once at `T0` and computes the diff against
that snapshot with no lock held, a concurrent `apply`/`destroy` can
make the displayed plan stale (a TOCTOU window — *not* a torn read;
both backends write state atomically). To bound this, `plan`:

1. Fingerprints state at `T0` (`StateFile::serial` + `lineage`) right
   after the initial read — `StateSnapshot::capture` in
   `carina-cli/src/commands/plan.rs`.
2. Re-reads state just before display and compares
   (`detect_state_drift`). A `lineage` change (state recreated) is
   reported in preference to a `serial` bump (concurrent write).
3. On drift, prints a **warning** to stderr and still shows the plan.
   Drift is never fatal: the plan is a prediction, and `apply`
   re-acquires the lock and recomputes the diff before mutating
   anything, so final correctness is enforced on the `apply` side.

`plan --out <file>` still writes the saved plan even when drift is
detected (the warning goes to stderr; the plan file is the command's
product). This is safe because the saved-plan apply path
(`run_apply_from_plan_locked`) records the plan's `state_lineage` /
`state_serial` and, under the apply lock, hard-errors on a lineage
mismatch and warns on a serial bump before mutating — so a stale saved
plan cannot be silently applied.

The contract is "best-effort snapshot with drift warning", not "locked
read". A failed re-read is also a warning, not an error. Regression
coverage: pure classification in
`commands::plan::state_drift_tests`; the real `carina plan` path
(concurrent writer slipped into the `T0..T1` window via a deterministic
file-handshake seam) in `carina-cli/tests/plan_state_drift_e2e.rs`.

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

### Worktree Workflow (REQUIRED)

- All PR work must happen in a git worktree, not the main repo.
- After `cd` into a worktree, verify cwd before running build/test commands.
- Never stash changes without explicit user instruction.
- Clean up worktrees after PR merge.

### Worktree-Based Development

```bash
# Create worktree for a new task
git worktree add .worktrees/<branch-name> -b <branch-name> main

# List worktrees
git worktree list

# Delete worktree after PR is merged (from the main worktree, not the feature worktree)
git worktree remove .worktrees/<branch-name>
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

## TDD for Bug Fixes

- Always write a failing reproducing test BEFORE implementing the fix.
- For TTY/rendering bugs, use PTY-based reproduction.
- Run full verify/review/CI gates before merge.

## Multi-Agent Workflows

Claude Code's `Workflow` tool runs a deterministic script that fans
work out across many subagents (parallel finders, adversarial
verifiers, pipeline stages). It can spawn dozens of agents and consume
a large amount of tokens, so it is **opt-in**: only run it when the
work explicitly calls for multi-agent orchestration. Default to a
single agent (or the `Agent` tool for one focused subtask) otherwise.

When a workflow IS warranted in this repo, these are the high-value
patterns:

- **Broad audits / sweeps.** AWS-error-display style sweeps,
  enum-alias/`snake_case` convention sweeps, or any "apply the same
  mechanical change across N sibling sites" task. Fan out one agent
  per file/site, verify each independently. Pairs with the
  root-cause rule: use a workflow to *find every sibling site* so the
  one upstream fix is provably complete, not to patch each site.
- **Blast-radius investigation before a typed reshape.** When deciding
  whether a newtype/typestate reshape lands in-PR or as a follow-up
  (see "Measure radius before deferring"), fan out readers across the
  affected crates to enumerate every call site in parallel. Prefer
  dagayn's `get_impact_radius` first; escalate to a workflow only when
  the graph result is ambiguous or needs source-level confirmation at
  many sites.
- **Multi-repo checks.** pick-issue / meta-tracker work that must scan
  all three carina-rs repos (carina, carina-provider-aws,
  carina-provider-awscc) — one agent per repo, then synthesize.
- **Adversarial self-review.** The 5-round self-review can be
  structured as a workflow: independent reviewers per dimension
  (correctness, type-safety, root-cause vs symptom), each finding
  verified by a skeptic prompted to refute it.

Constraints specific to this repo:

- A workflow does NOT relax any gate. Each agent still runs the
  crate-scoped verify cycle (`cargo check -p` → `cargo nextest run` →
  doctests → clippy → `scripts/check-*.sh`), and findings still need a
  reproducing test before a fix lands.
- Agents that mutate files in parallel must use `isolation: 'worktree'`
  to avoid clobbering each other; read-only finders do not need it.
- Have agents return structured output (the `schema` option) rather
  than prose when you will post-process their results.

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
