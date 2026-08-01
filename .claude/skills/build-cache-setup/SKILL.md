---
name: build-cache-setup
description: One-time build cache setup for the Carina repo — sccache, sccache-wrapper for cross-worktree reuse, per-worktree target dirs, and mitigations for multi-worktree parallel verify contention. Use when setting up a new machine or worktree, or when builds feel slow with several worktrees active.
---

# Build Cache Setup (sccache, per-worktree target)

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

## Cross-Worktree Caching with sccache-wrapper (recommended)

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

## Multi-Worktree Parallel Verify

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
