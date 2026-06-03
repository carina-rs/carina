# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

---

# Part 1 — Core Rules

These are the rules that override default behavior. Read them first.

## Root-cause fixes only — and make the broken state unrepresentable

**The most important rule of this project.** A bug fix must answer two
questions, and passing the first is not evidence of passing the second:

1. **Root cause** — *what is broken right now?* Fix the *one* upstream
   change that restores the invariant, not a filter / guard / carve-out
   at every consumer site. If the same broken invariant produces
   symptoms in multiple code paths (`apply`, `destroy`, `state refresh`,
   `plan`, …), they are *one* bug, not N.
2. **Long-term / type safety** — *can a future caller re-reach this
   class of bug?* Prefer the fix that makes the broken state
   unrepresentable (newtypes, tagged unions, typestate) over a runtime
   filter every consumer must remember.

A fix that answers yes-to-(1) but no-to-(2) is **a runtime patch at
multiple consumer sites disguised as a root-cause fix** — it works today
and silently regresses when the next consumer is added.

**Self-checks before opening a PR:**

- If the diff *filters / guards / skips* the buggy condition instead of
  *removing* it, the fix is symptom-level. Step back, find the upstream seam.
- Does the diff add `find_*` / `resolve_*` / `lookup_*` at multiple
  consumer sites? Does it rely on every consumer remembering to do
  something? Is a sibling code path doing the same dance? Any "yes" →
  a newtype / typestate is the right factoring tool. The "new caller
  tomorrow" check must be answered by the **type signature**, not by
  documentation or convention. Make the resolver step required by the
  type: return a wrapper only a resolver can produce; make the raw type
  uncomparable to the resolved type.
- **5-round review passing is NOT evidence the fix is root-cause.** A
  bandaid passes every gate. Ask "if a new caller appears tomorrow, does
  it need to remember this filter too?" — if yes, the root is still broken.

**Never** propose "minimal fix in this PR, follow-up issue for the rest"
when "the rest" is the same class of bug at sibling sites — that is a
bandaid presented as scope discipline. **Never** invoke "1 PR = 1 topic"
to justify a per-site patch; "1 topic" means one *root cause*. Fixing the
root *is* the topic. **When in doubt, pick the broader fix** — shrinking
scope and offering a follow-up has been pushed back on every single time.

**Measure radius before deferring the typed reshape.** The temptation to
defer is strongest when the runtime patch is in front of you and the
reshape feels big. Stub the newtype, run
`cargo check --workspace --all-targets 2>&1 | grep error | wc -l`,
revert. A small number (single / low double digits) → **do it in-PR**;
the carina#3280 lesson is that "wide blast radius" intuitions have
repeatedly been wrong. Only when the radius is *genuinely* large, file
the type-level follow-up **in the same response** as the runtime fix PR —
referencing the runtime PR and the remaining type hazard explicitly — not
"I might file it later".

> Past failure mode (carina#3324 / PR #3325 → carina#3326): a runtime
> resolver fix at three consumer sites passed five review rounds and
> merged as "root-cause". The user then asked whether it was type-safe.
> Honest answer: no — `ResourceId` still permitted a routing-mismatch
> comparison, and a fourth consumer would re-introduce the bug. The
> typed reshape (`StateBlockAddress` newtype) became a follow-up instead
> of landing in the same PR. The lesson: apply *both* lenses at PR
> creation, not after the user asks.

## Delegate hands-on work to Codex

Work that edits files — implementation, refactoring, and plan-writing —
is delegated to Codex via the **codex** skill. Opus does not edit files
directly; it reads, thinks, reviews, and writes docs only.

- **When**: any implementation, refactor/cleanup, or plan-writing task,
  and escalation after repeated failed fix attempts.
- **How**: invoke the codex skill and follow its process — carry the
  intent/essence down, delegate with concrete scope, review the draft
  (root cause first), and send findings back to Codex to fix.
- Opus writing the plan, the implementation, or the refactor edits
  itself is forbidden; docs are the only thing Opus writes.

See the codex skill for the full subcontracting protocol.

## Communication Style

- Be terse. Do not ask permission for obvious actions (e.g., using the correct AWS profile, standard build commands).
- When opening/merging a PR, always include the GitHub URL directly in the response.
- No sycophantic phrasing ('Great question!', 'You're absolutely right!').
- Do not over-explain hypothetical design options before checking the actual real-world case (screenshots, real output).

## 日本語の文章作法

日本語のドキュメントを書く・編集するときだけでなく、会話で日本語を使う
ときも、次の作法に従う。

- **考える段階から日本語に寄せる。** 英語で考えてから訳すと横文字が増えたり訳文調に
  なったりしやすい。日本語の単語を使っていても言い回しだけが英語に引っ張られることが
  あるので注意する。ただし、その分野で普通に通じる言葉まで無理に言い換えなくてよい。
  ソフトウェアで定着した語（リリース、コミット、マージ、デプロイなど）はそのまま使う。
  逆に、定着した訳がないものを直訳すると不自然になることがある（例: ship を「出荷」と
  訳すより「リリース」のほうが通じる）。横文字を日本語に寄せることと、通用語をそのまま
  使うことの兼ね合いで判断する。
- **強調は最小限にする。** 太字は用語の初出定義や注記ラベルくらいにとどめ、本文中の
  単語を強調しすぎない。
- **箇条書きを続けすぎない。** 箇条書きが続くと機械的な文章に見えやすい。散文でつなげ
  られるところは自然な文章としてつなげる。ただし禁止ではなく、並列の項目や手順、対応
  関係など、箇条書きや表のほうが見やすい場面では適切に使ってよい。避けすぎて読みにくく
  散文へ流し込むのも同じく良くない。
- **何をしているかが分かる言葉を使う。** 専門用語はその場で短く説明しながら使い、読み手が
  当然知っているものとしていきなり置かない。また抽象的な言い回しで誤魔化さない。たとえば
  「安価な防御」では何をしているか分からない。「入口での形式チェックと流量制限」のように、
  実際に行う操作を指す言葉を選ぶ。
- **ファイル名・パス・コード上の名前を本文にそのまま出しすぎない。** 共有資料やスライドに
  なったときに伝わりにくいことがある。必要に応じて文書タイトルや概念名に置き換える。
- **囲み枠や注記ブロックを多用しない。** 自然な本文に溶かせる内容なら散文の中で扱う。
  ただし、本文から切り出して目立たせる理由がある場合（警告、重要な例外、補足など）は
  使ってよい。

## Code Style

- **Commit messages**: Write in English
- **Code comments**: Write in English

## TDD for Bug Fixes

- Always write a failing reproducing test BEFORE implementing the fix.
- For TTY/rendering bugs, use PTY-based reproduction.
- When bugs are found or issues are pointed out, write a test that captures the fix so the regression is caught and the expected behavior is documented.
- Run full verify/review/CI gates before merge.

---

# Part 2 — Build, Test, Verify

## Commands

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

## Verify Protocol — Do Not Run Redundant Builds

The verify cycle is the slowest thing about working on this repo. The
single biggest waste is running `cargo build` immediately before
`cargo test`: the test step compiles the same artifacts the build step
just produced.

**Rules:**

- **Do not run `cargo build`** as a separate verification step.
  `cargo nextest run` (or `cargo test`) already compiles everything it needs.
- For a faster compile-only sanity check during iteration, use
  `cargo check -p <crate>` (skips linking, ~30–50% faster than `cargo build`).
- The only legitimate use of `cargo build` in the verify cycle is
  `cargo build --release` immediately before opening a PR, to catch
  release-only issues that debug builds miss. Skip it for refactors,
  bug fixes, or anything that has not changed `Cargo.toml` / unsafe code /
  the `release` profile config.

**Verify cycle order** (from #2289 / #2291):

```bash
# 1. Compile-only sanity (crate-scoped during iteration)
cargo check -p <crate>

# 2. Tests, scoped to what changed (see touched-crates.sh below)
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

Broaden step 2 to `cargo nextest run --workspace` only when the change
spans crates. Steps 3–5 stay workspace-scoped: they are fast and catch
cross-crate doctest references, workspace-level lint config, and repo-wide
invariants that a crate-scoped run misses. The `cargo test --doc` step is
cheap once nextest has compiled the crates — public API doctests in
`carina-core/src/utils.rs` and elsewhere are not covered by nextest, so
always run it before declaring verify done.

> CI's `Test` job runs `cargo build -p carina-provider-mock --target
> wasm32-wasip2` *before* the test step, but that build targets a
> different platform than the test step (host), so it is not redundant —
> it produces the WASM fixture that `carina-plugin-host`'s integration
> tests load. Do not generalize from that step to local development.

## Incremental Build Strategy

When working on a specific crate, use crate-specific commands to avoid
unnecessary compilation:

- After modifying a single crate, test only that crate with
  `cargo nextest run -p <crate-name>`. Do **not** run
  `cargo build -p <crate-name>` first — the test command does the build.
- Use full workspace `cargo nextest run` only when changes affect multiple
  crates or before creating a PR; follow it with `cargo test --workspace --doc`.
- For the fastest iteration loop, `cargo check -p <crate-name>` skips linking.
- Provider crates (aws, awscc) are in separate repositories — changes here
  may require updating those repos.

### Crate-Scoped Verify Helper

`scripts/touched-crates.sh` maps changed files to the cargo `-p <crate>`
flags that exercise them, so iteration verify can stay crate-scoped:

```bash
# What crates does the current branch touch (vs origin/main)?
scripts/touched-crates.sh
# → "-p carina-core -p carina-cli"   (or "--workspace", or empty)

cargo nextest run $(scripts/touched-crates.sh)   # run only touched crates
scripts/touched-crates.sh --base main            # diff against a specific base
git diff --name-only | scripts/touched-crates.sh --stdin   # custom file list
```

| Output           | Meaning                                                   |
| ---------------- | --------------------------------------------------------- |
| `-p <crate> ...` | Run only those crates' tests; transitive consumers are not invalidated. |
| `--workspace`    | Cross-cutting change (touched `carina-core`, root `Cargo.toml`/`Cargo.lock`, or an unrecognized path). Sweep the whole workspace. |
| (empty)          | No test-relevant files changed (only docs / CI / scripts / infra). Skip the test step. |

The helper is intentionally pessimistic. Two "fall back to `--workspace`"
rules: **(1) `carina-core` touched** — every other crate depends on it, so
testing only `carina-core` would miss downstream regressions; **(2) unknown
path** — a new top-level directory or unclassified file is treated as
workspace-wide until the helper learns about it.

For rigorous transitive impact analysis ("which crates' *tests* are affected
when I change a function in `carina-core`?"), use dagayn's `get_impact_radius`,
which walks the call graph rather than relying on the directory mapping.

## Plan Display Testing

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

When plan output changes intentionally, update snapshots with
`cargo insta review` (interactive) or `cargo insta accept` (accept all).
If snapshots are not updated after a display change, CI fails on the `Test` job.

## Build Cache Setup (sccache, per-worktree target)

To speed up builds across git worktrees, set up sccache. Each worktree
keeps its own `target/` directory; sccache provides cross-worktree reuse
at the rustc-invocation level.

```bash
brew install sccache
mkdir -p .cargo
cat > .cargo/config.toml << 'EOF'
[build]
rustc-wrapper = "sccache"
EOF
```

Why this shape:

- **sccache** caches compiled artifacts by content hash globally. New
  worktrees hit the cache at the rustc-call level instead of recompiling
  dependencies from scratch — that is where cross-worktree reuse comes from.
- **Per-worktree `target/`** (the cargo default — no `target-dir` override)
  keeps each worktree's incremental build state local. Cargo locks the
  target directory while building, so a single shared `target-dir` across
  worktrees serializes parallel work and produces "Blocking waiting for
  file lock on artifact directory" stalls when multiple agents run at once.

Earlier guidance recommended `target-dir = "/Users/mizzy/.cargo-target/carina"`.
That is now discouraged — drop the `target-dir = ...` line if you have it.
This is currently a pilot (#2290); the per-worktree shape is the new default,
but real wall-clock numbers will be collected over the next few PR cycles.

Note: `.cargo/config.toml` is gitignored because it contains machine-specific
paths. Each new worktree needs the file copied or recreated.

### Cross-Worktree Caching with sccache-wrapper (recommended)

Plain sccache mixes the absolute source path into its cache key, so a
second worktree at the *same commit* still misses many entries — even
though the source is byte-identical. On a `carina-core` cold→warm benchmark
the second worktree took 7.6s with plain sccache because rustc still
recompiled most crates.

[`sccache-wrapper`](https://github.com/moriyoshi/sccache-wrapper) is a
`RUSTC_WRAPPER` that normalizes the workspace root to a `@@WORKSPACE@@`
placeholder *before* computing the cache key, then delegates to sccache.
With it, worktree B of the same benchmark hit the cache for all 52 crates
and finished in 1.6s — a ~4.8x wall-clock win on the second-and-later worktree.

```bash
# Build the wrapper. Unset RUSTC_WRAPPER first so it doesn't recurse into itself.
git clone --depth 1 https://github.com/moriyoshi/sccache-wrapper.git /tmp/sccache-wrapper
( cd /tmp/sccache-wrapper && RUSTC_WRAPPER= cargo build --release )
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
- Leave `WB_WORKSPACE_ROOT` unset. The wrapper derives it per invocation via
  `git rev-parse --show-toplevel`, so the same config works in every worktree.
  Set it explicitly only to skip that subprocess overhead.
- The wrapper strips `-C incremental=…`. For a tight edit-rebuild loop inside
  a *single* worktree, plain incremental builds can be faster; the wrapper's
  win is concentrated on the second-and-later worktree (a cold worktree sees
  little benefit, ~8.4s vs ~9.6s).
- `WB_RUSTC_CACHE_DEBUG=1` logs per-crate HIT/MISS; `sccache-wrapper
  --dump-cache` lists all cache entries.

Like the per-worktree `target/` change, this is a pilot (#2290).

### Multi-Worktree Parallel Verify

When 2+ worktrees run `cargo nextest run` (or any cargo build) at the same
time, each verify cycle gets slower. Contention sources, in order of severity:

1. **sccache file-storage lock contention** — the default backend serializes
   concurrent writers, so cross-worktree reuse stalls instead of accelerating.
2. **Duplicate dependency compilation** — per-worktree `target/` removes
   cargo's file-lock stalls but does not dedupe compile work; on a cache miss
   each worktree recompiles the same dependency graph.
3. **rustc / linker CPU + memory-bandwidth contention** — each worktree spawns
   its own rustc/linker processes; linking is especially bandwidth-heavy.

Mitigations, in order to try them:

1. **Scope tests to touched crates** (biggest win, applies even with one
   worktree). Use `scripts/touched-crates.sh` instead of defaulting to
   `--workspace`; prefer `cargo check -p <crate>` for mid-iteration sanity.
2. **Cap test parallelism per worktree with `cargo nextest run -j N`** when
   multiple worktrees are active. Pick `N` so the total across worktrees stays
   at or below the physical core count (e.g. 16-core / 2 worktrees → `-j 8`
   each). This caps *test execution* only, not compile; rustc/linker contention
   is unaffected. No `.config/nextest.toml` default — a fixed cap penalizes the
   common single-worktree case.
3. **Switch sccache to a Redis backend (opt-in)** — removes the file-storage
   lock contention (source 1) and improves cross-worktree hit rate (reducing
   source 2). Opt-in because Redis is a long-running service and the benefit
   only materializes with 2+ worktrees compiling concurrently.

```bash
brew install redis
brew services start redis
export SCCACHE_REDIS_ENDPOINT=redis://127.0.0.1:6379   # add to your shell rc
sccache --stop-server && sccache --start-server        # restart to pick up the backend
sccache --show-stats | grep -E "^Cache (hits|misses|hits rate)"
```

Cap Redis memory via `maxmemory` + `maxmemory-policy allkeys-lru` in
`redis.conf` (`/opt/homebrew/etc/` on Apple Silicon brew, `/usr/local/etc/`
on Intel). `SCCACHE_CACHE_SIZE` only applies to the local file backend; with
Redis the bound is set on the Redis side. Unset `SCCACHE_REDIS_ENDPOINT` to
revert — sccache falls back to its default file storage automatically.

---

# Part 3 — Architecture

Carina is a functional infrastructure management tool that treats side effects as values (Effects) rather than immediately executing them.

## Data Flow

```
DSL (.crn) → Parser → Resources → Differ → Plan (Effects) → Provider → Infrastructure
```

## Key Abstractions

- **Effect** (`carina-core/src/effect.rs`): Enum representing side effects (Create, Update, Delete, Read). Effects are values, not executed operations.
- **Plan** (`carina-core/src/plan.rs`): Collection of Effects. Immutable, can be inspected before execution.
- **Provider** trait (`carina-core/src/provider.rs`): Async trait for infrastructure operations. Returns `BoxFuture` for async methods.

## Repository Structure (Polyrepo)

Carina is split across multiple repositories under [carina-rs](https://github.com/carina-rs):

| Repository | Description |
|------------|-------------|
| **carina** (this repo) | Core, CLI, LSP, plugin SDK/host, state, TUI |
| [carina-provider-aws](https://github.com/carina-rs/carina-provider-aws) | AWS provider (Smithy-based codegen) |
| [carina-provider-awscc](https://github.com/carina-rs/carina-provider-awscc) | AWS Cloud Control provider |

Provider repositories depend on this repo's crates via `git` dependencies.

## Crate Structure (this repo)

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

## DSL Parser

The parser uses [pest](https://pest.rs/) grammar defined in `carina-core/src/parser/carina.pest`. Key constructs:
- `provider <name> { ... }` - Provider configuration
- `<provider>.<service>.<resource_type> { ... }` - Anonymous resource (ID from `name` attribute)
- `let <binding> = <resource>` - Named resource binding

## LSP Integration

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
    `carina-core/tests/tmlanguage_keyword_parity.rs` fails the build otherwise.

## Resource Type Mapping

Resource types in DSL use dot notation (`s3.bucket`, `ec2.vpc`). When mapping between DSL resource types and schema lookups:
- DSL: `aws.s3.Bucket` → Schema key: `s3.bucket`
- Ensure `extract_resource_type()` in `completion/mod.rs` and resource type validation in `diagnostics/mod.rs` use consistent dot notation

## Validation Formats

- **Region**: Accepts both DSL format (`aws.Region.ap_northeast_1`) and AWS string format (`"ap-northeast-1"`). Validation normalizes both to AWS format for comparison.
- **S3 Versioning**: Uses enum `aws.s3.VersioningStatus.Enabled` / `aws.s3.VersioningStatus.Suspended` in DSL (PascalCase matches the AWS SDK representation).

## Namespaced Enum Identifiers

Enum values use namespaced identifiers like `aws.s3.Bucket.VersioningStatus.enabled`.

**When adding new namespaced patterns:**

1. **Pattern matching must handle digits** - Resource names like `s3` contain digits. Use `c.is_ascii_digit()` in addition to `c.is_lowercase()`:
   ```rust
   // Wrong: "s3" fails because '3'.is_lowercase() == false
   resource.chars().all(|c| c.is_lowercase() || c == '_')

   // Correct:
   resource.chars().all(|c| c.is_lowercase() || c.is_ascii_digit() || c == '_')
   ```

2. **Treat `TypeIdentity` as the source of truth when one is available.**
   `NamespacedId::parse` in `carina-core/src/utils.rs` remains the
   schema-free syntactic gate for namespaced-looking values, and its 5+
   part branch still pins `TypeName` at index 3 so dotted values
   (`ipsec.1`) flow into the trailing slice. Validation paths that have
   an identity, such as `validate_enum_namespace`, must compare against
   the identity's provider, structural segments, and kind rather than
   re-deriving those axes from the dotted display string. Deep
   multi-segment full paths validate by this structural identity match.

3. **Plan display should not quote namespaced identifiers** - They are identifiers, not strings

4. **LSP diagnostics must validate Custom types** - When adding `AttributeType::Custom` with a validate function, ensure `carina-lsp/src/diagnostics/mod.rs` calls the validate function for editor warnings

5. **Always test with actual values** - Don't assume pattern matching works; write a quick test to verify

## Struct Types

`AttributeType::Struct` represents nested objects with typed fields.

- Defined in `carina-core/src/schema.rs` as `Struct { name, fields: Vec<StructField> }`
- Each `StructField` has: `name`, `field_type` (recursive AttributeType), `required`, `description`
- **LSP integration**: when adding Struct validation, update
  `carina-lsp/src/diagnostics/validation.rs` to validate nested fields;
  completion should work recursively for struct fields.

## Module Loading

Modules are directory-scoped. An import target must be a directory containing
one or more `.crn` files; single-file modules are rejected with
`ModuleError::NotADirectory`. Inside a module directory all `.crn` files are
merged uniformly — no file name (including `main.crn`) is privileged.

- CLI: `load_module()` / `ModuleResolver::load_module` require a directory path.
- LSP: Module loading in `diagnostics/checks.rs` handles directory modules for proper validation.

## Plan Concurrency Contract

<!-- constrained-by #key-abstractions -->

`carina plan` **takes no state lock.** `apply`/`destroy` acquire an
exclusive lock (`backend.acquire_lock(...)`); `plan` deliberately does
not. A lock on a read-only operation is overkill, and because the
backend lock API is exclusive-only it would serialize concurrent
`plan`s and let a long `plan` block deploys — too strong for a command
whose output is only a prediction. (Deliberate decision for issue #3111;
options considered were: (1) document as intentional, (2) acquire a
shared/read lock, (3) detect drift and warn. Option 3 was chosen — option
1 is weaker than Terraform and leaves the user unaware of staleness;
option 2 needs a shared-lock mode the backends do not have and is only
justified once a saved-plan/`plan -out`→`apply` workflow exists.)

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
coverage: pure classification in `commands::plan::state_drift_tests`;
the real `carina plan` path (concurrent writer slipped into the `T0..T1`
window via a deterministic file-handshake seam) in
`carina-cli/tests/plan_state_drift_e2e.rs`.

## Directory-scoped, never single-file

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

> Past breakage: PR #2120 (closed #2043 too early) shipped `exports`
> value-position completion that scanned only the current buffer; the real
> `exports.crn` references bindings in `main.crn`, so users got zero LSP
> candidates. PR #2121 fixed it by threading `base_path` into the same
> handler. Same class of mistake recurred from PR #2118 (closed #2117
> prematurely) where the formatter fix passed unit tests but never ran
> against the real infra fixture.

---

# Part 4 — Git & Worktree Workflow

## Worktree Workflow (REQUIRED)

- All PR work must happen in a git worktree, not the main repo.
- After `cd` into a worktree, verify cwd before running build/test commands.
- Never stash changes without explicit user instruction.
- Clean up worktrees after PR merge.

```bash
# Create worktree for a new task
git worktree add .worktrees/<branch-name> -b <branch-name> main

git worktree list

# Delete worktree after PR is merged (from the main worktree, not the feature worktree)
git worktree remove .worktrees/<branch-name>
```

## Submodule Initialization

This repo uses a git submodule for `carina-plugin-wit/`. After `git pull` or creating a new worktree, initialize the submodule:

```bash
git submodule update --init --recursive
```

Without this, builds fail because `wit_bindgen::generate!` cannot find the WIT files.

## Branch Cleanup

After merging a PR:
```bash
git checkout main
git pull
git branch -d <feature-branch>    # Delete local branch
git remote prune origin           # Remove stale remote tracking branches
```

---

# Part 5 — Multi-Agent Workflows

Claude Code's `Workflow` tool runs a deterministic script that fans
work out across many subagents (parallel finders, adversarial
verifiers, pipeline stages). It can spawn dozens of agents and consume
a large amount of tokens, so it is **opt-in**: only run it when the
work explicitly calls for multi-agent orchestration. Default to a
single agent (or the `Agent` tool for one focused subtask) otherwise.

High-value patterns in this repo:

- **Broad audits / sweeps.** AWS-error-display style sweeps,
  enum-alias/`snake_case` convention sweeps, or any "apply the same
  mechanical change across N sibling sites" task. Fan out one agent
  per file/site, verify each independently. Pairs with the root-cause
  rule: use a workflow to *find every sibling site* so the one upstream
  fix is provably complete, not to patch each site.
- **Blast-radius investigation before a typed reshape.** When deciding
  whether a newtype/typestate reshape lands in-PR or as a follow-up,
  fan out readers across the affected crates to enumerate every call site
  in parallel. Prefer dagayn's `get_impact_radius` first; escalate to a
  workflow only when the graph result is ambiguous or needs source-level
  confirmation at many sites.
- **Multi-repo checks.** pick-issue / meta-tracker work that must scan
  all three carina-rs repos — one agent per repo, then synthesize.
- **Adversarial self-review.** The 5-round self-review can be structured
  as a workflow: independent reviewers per dimension (correctness,
  type-safety, root-cause vs symptom), each finding verified by a skeptic
  prompted to refute it.

Constraints specific to this repo:

- A workflow does NOT relax any gate. Each agent still runs the
  crate-scoped verify cycle (`cargo check -p` → `cargo nextest run` →
  doctests → clippy → `scripts/check-*.sh`), and findings still need a
  reproducing test before a fix lands.
- Agents that mutate files in parallel must use `isolation: 'worktree'`
  to avoid clobbering each other; read-only finders do not need it.
- Have agents return structured output (the `schema` option) rather
  than prose when you will post-process their results.

---

# Part 6 — Tooling

## MCP Tools: dagayn

<!-- dagayn MCP tools -->

**This project has a knowledge graph. ALWAYS use the dagayn MCP tools
BEFORE Grep/Glob/Read to explore the codebase** — the graph is faster,
cheaper, and gives structural context (callers, dependents, test coverage)
that file scanning cannot. The full tool surface, the FIRST-choice tool
table, the drill-down table, and the recommended workflow live in the
global `~/.claude/CLAUDE.md` "MCP Tools: dagayn" section — follow that.
The short version for this repo:

- Start any new task with `get_minimal_context(task=...)`.
- Review changes with `detect_changes` (read its `analysis_summary` first);
  pull source snippets with `get_review_context`.
- Trace relationships with `query_graph` (callers_of / callees_of /
  imports_of / tests_for); check coverage with pattern="tests_for" before
  claiming a path is untested.
- Understand blast radius with `get_impact_radius`; architecture with
  `get_architecture_overview` (read `architecture_health` first).
- Fall back to Grep/Glob/Read only when the graph result is missing, stale,
  ambiguous, or lacks the exact source text needed.

## Markdown documentation policy

<!-- dagayn markdown policy -->

The full policy — declaring inter-section / inter-document dependencies as
`<!-- <kind> <target> -->` directive comments so dagayn captures
`DEPENDS_ON` / `IMPORTS_FROM` edges — is defined in the global
`~/.claude/CLAUDE.md` "Markdown documentation policy" section and applies
verbatim here. In brief: `<kind>` ∈ {`constrained-by`, `blocked-by`,
`supersedes`, `derived-from`}; targets are `#section-slug`,
`./path.md`, or `./path.md#slug`; place the directive immediately under the
heading that depends on the target; do not invent dependencies.
