# Phase 4: Remove Direct Provider Dependencies — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Remove `carina-provider-aws` and `carina-provider-awscc` as direct dependencies of `carina-cli`, making all providers external processes. `source` and `version` become required for aws/awscc provider blocks.

**Architecture:** Change `WiringContext` from hardcoded factory registration to dynamic factory building. For each provider with `source`, resolve the binary via `provider_resolver` and create a `ProcessProviderFactory`. The existing factory-based schema/normalizer/validation infrastructure continues to work through the `ProviderFactory` trait interface. Provider blocks without `source` (except mock) produce a clear error.

**Tech Stack:** `carina-plugin-host::ProcessProviderFactory`, `carina-cli::provider_resolver`

**Spec:** `docs/superpowers/specs/2026-03-31-remove-direct-provider-deps-design.md`

**Important:** `aws-config`, `aws-sdk-kms`, `aws-sdk-s3` remain in carina-cli — they're used for S3 state backend and KMS decryption, not for providers.

---

## File Structure

### Modified Files

```
carina-cli/Cargo.toml                          — Remove carina-provider-aws, carina-provider-awscc deps
carina-cli/src/wiring.rs                       — Dynamic WiringContext, remove hardcoded factory lookup
carina-cli/src/main.rs                         — Remove provider factory imports
carina-cli/src/tests.rs                        — Update test WiringContext usage
carina-cli/tests/fixtures/**/*.crn             — Add source/version to provider blocks (where applicable)
carina-lsp/Cargo.toml                          — Remove carina-provider-aws, carina-provider-awscc deps
carina-lsp/src/main.rs                         — Remove hardcoded factory instantiation
carina-lsp/src/completion/tests/mod.rs          — Update test factory setup
carina-lsp/src/diagnostics/tests/mod.rs         — Update test factory setup
```

---

## Task 1: Make WiringContext accept dynamic factories

The core change. Instead of hardcoding `AwsProviderFactory` and `AwsccProviderFactory` in `WiringContext::new()`, make it accept factories as a parameter.

**Files:**
- Modify: `carina-cli/src/wiring.rs`

- [ ] **Step 1: Change `WiringContext::new()` to accept factories**

In `carina-cli/src/wiring.rs`, change:

```rust
impl WiringContext {
    pub fn new() -> Self {
        let factories: Vec<Box<dyn ProviderFactory>> =
            vec![Box::new(AwsProviderFactory), Box::new(AwsccProviderFactory)];
        let schemas = provider_mod::collect_schemas(&factories);
        Self { factories, schemas }
    }
```

to:

```rust
impl WiringContext {
    pub fn new(factories: Vec<Box<dyn ProviderFactory>>) -> Self {
        let schemas = provider_mod::collect_schemas(&factories);
        Self { factories, schemas }
    }
```

- [ ] **Step 2: Create a builder function that resolves providers into factories**

Add a new function in `wiring.rs`:

```rust
/// Build provider factories from resolved provider configs.
///
/// For each provider with `source`, creates a ProcessProviderFactory.
/// Providers without `source` are skipped (handled as error later in get_provider_with_ctx).
pub fn build_factories_from_providers(
    providers: &[ProviderConfig],
    base_dir: &Path,
) -> Vec<Box<dyn ProviderFactory>> {
    let mut factories: Vec<Box<dyn ProviderFactory>> = Vec::new();

    for config in providers {
        let source = match &config.source {
            Some(s) => s.as_str(),
            None => continue,
        };

        let binary_path = if let Some(path) = source.strip_prefix("file://") {
            std::path::PathBuf::from(path)
        } else if source.starts_with("github.com/") {
            match crate::provider_resolver::resolve_single_config(base_dir, config) {
                Ok(path) => path,
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("Failed to resolve provider '{}': {}", config.name, e).red()
                    );
                    continue;
                }
            }
        } else {
            continue;
        };

        match carina_plugin_host::ProcessProviderFactory::new(binary_path) {
            Ok(factory) => {
                factories.push(Box::new(factory));
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Failed to load provider '{}': {}", config.name, e).red()
                );
            }
        }
    }

    factories
}
```

- [ ] **Step 3: Update `get_provider_with_ctx` to error on source-less non-mock providers**

In the provider loop inside `get_provider_with_ctx`, replace the hardcoded factory lookup:

```rust
        // Otherwise, use the hardcoded factory lookup (existing behavior)
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &provider_config.name) {
            ...
        }
```

with:

```rust
        // Provider without source — check if we have a factory from resolved providers
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &provider_config.name) {
            let region = factory.extract_region(&provider_config.attributes);
            println!(
                "{}",
                format!("Using {} (region: {})", factory.display_name(), region).cyan()
            );
            let provider = factory.create_provider(&provider_config.attributes).await;
            router.add_provider(provider_config.name.clone(), provider);
            if let Some(ext) = factory.create_normalizer(&provider_config.attributes).await {
                router.add_normalizer(ext);
            }
        } else {
            eprintln!(
                "{}",
                format!(
                    "Provider '{}' requires 'source' and 'version' attributes. Example:\n  source = \"github.com/carina-rs/carina-provider-{}\"",
                    provider_config.name, provider_config.name
                ).red()
            );
        }
```

Note: This preserves the existing factory lookup for any ProcessProviderFactories that were registered. The difference is that no hardcoded factories exist — only dynamically built ones.

- [ ] **Step 4: Update all callers of `WiringContext::new()`**

Search for `WiringContext::new()` in carina-cli/src/ and update each call site. There are two patterns:

**Pattern A — commands that have `parsed` and `base_dir` available (plan, apply, destroy, state):**
```rust
let factories = build_factories_from_providers(&parsed.providers, base_dir);
let ctx = WiringContext::new(factories);
```

**Pattern B — `create_providers_from_configs` (apply from plan file):**
```rust
let factories = build_factories_from_providers(configs, base_dir);
let ctx = WiringContext::new(factories);
```

For each call site, check what variables are available and pass the providers + base_dir.

- [ ] **Step 5: Verify build (expect compile errors from missing imports — that's OK)**

Run: `cargo check -p carina-cli 2>&1 | head -20`

At this point the code should compile if the imports still exist. We'll remove them in a later task.

- [ ] **Step 6: Commit**

```bash
git add carina-cli/src/wiring.rs
git commit -m "refactor: make WiringContext accept dynamic factories, add build_factories_from_providers"
```

---

## Task 2: Remove `carina-provider-aws` and `carina-provider-awscc` from carina-cli

**Files:**
- Modify: `carina-cli/Cargo.toml`
- Modify: `carina-cli/src/wiring.rs` (remove imports)
- Modify: `carina-cli/src/main.rs` (remove imports if any)

- [ ] **Step 1: Remove dependencies from Cargo.toml**

In `carina-cli/Cargo.toml`, remove these lines from `[dependencies]`:

```toml
carina-provider-aws = { path = "../carina-provider-aws" }
carina-provider-awscc = { path = "../carina-provider-awscc" }
```

- [ ] **Step 2: Remove imports from wiring.rs**

Remove these lines from `carina-cli/src/wiring.rs`:

```rust
use carina_provider_aws::AwsProviderFactory;
use carina_provider_awscc::AwsccProviderFactory;
```

- [ ] **Step 3: Fix any remaining compile errors**

Run: `cargo check -p carina-cli 2>&1`

Fix any remaining references to `AwsProviderFactory`, `AwsccProviderFactory`, or imports from those crates. These might be in test code or other command files.

- [ ] **Step 4: Verify build**

Run: `cargo build -p carina-cli`
Expected: BUILD SUCCESS

- [ ] **Step 5: Commit**

```bash
git add carina-cli/Cargo.toml carina-cli/src/
git commit -m "feat: remove carina-provider-aws and carina-provider-awscc from carina-cli"
```

---

## Task 3: Update test fixtures and test code

Test fixtures that use `provider awscc { ... }` or `provider aws { ... }` without `source` need to be updated. However, many fixture `.crn` files are used for plan display testing with the mock provider — they don't actually instantiate real providers.

**Files:**
- Modify: `carina-cli/src/tests.rs`
- Modify: Various `.crn` fixture files (as needed)

- [ ] **Step 1: Run all tests to identify failures**

Run: `cargo test -p carina-cli 2>&1 | grep -E "FAILED|error" | head -30`

Review which tests fail and why. Common patterns:
- Tests that call `WiringContext::new()` with no args → need to pass `vec![]` or mock factories
- Tests that use fixture `.crn` files with provider blocks → may need source/version or may work if mock fallback handles them

- [ ] **Step 2: Fix test code**

For tests that create `WiringContext::new()`, update to `WiringContext::new(vec![])` or provide appropriate factories.

For tests that need provider schemas (e.g., plan_snapshot tests), they may need to either:
- Use `file://` source pointing to the built provider binary
- Or use empty factories (if the test doesn't need provider schemas)

- [ ] **Step 3: Update fixture `.crn` files if needed**

Only update fixture files that cause test failures. Many fixtures may continue to work with the mock provider fallback.

- [ ] **Step 4: Verify all tests pass**

Run: `cargo test -p carina-cli`
Expected: All tests PASS

- [ ] **Step 5: Commit**

```bash
git add carina-cli/src/ carina-cli/tests/
git commit -m "fix: update tests for dynamic provider factory loading"
```

---

## Task 4: Remove direct provider deps from LSP

The LSP currently hardcodes `AwsProviderFactory` and `AwsccProviderFactory`. For Phase 4, remove these and let the LSP work with empty factories (reduced functionality — no provider-specific completions until a provider binary is available).

**Files:**
- Modify: `carina-lsp/Cargo.toml`
- Modify: `carina-lsp/src/main.rs`
- Modify: `carina-lsp/src/completion/tests/mod.rs`
- Modify: `carina-lsp/src/diagnostics/tests/mod.rs`

- [ ] **Step 1: Remove dependencies from LSP Cargo.toml**

In `carina-lsp/Cargo.toml`, remove:

```toml
carina-provider-aws = { path = "../carina-provider-aws" }
carina-provider-awscc = { path = "../carina-provider-awscc" }
```

- [ ] **Step 2: Update LSP main.rs**

Remove the hardcoded factory instantiation and replace with empty vec:

```rust
let factories: Vec<Box<dyn ProviderFactory>> = vec![];
```

Also remove any imports of `AwsProviderFactory`, `AwsccProviderFactory`, and `awscc_validators`.

- [ ] **Step 3: Fix LSP test code**

Update test files that reference `AwsProviderFactory` / `AwsccProviderFactory` to use empty factories or adjust test expectations.

- [ ] **Step 4: Verify LSP builds and tests pass**

Run: `cargo build -p carina-lsp && cargo test -p carina-lsp`
Expected: BUILD SUCCESS, tests PASS (some tests may need adjustment if they relied on specific provider schemas)

- [ ] **Step 5: Commit**

```bash
git add carina-lsp/
git commit -m "refactor: remove hardcoded provider factories from LSP"
```

---

## Task 5: Validate — full workspace build and test

**Files:**
- Potentially any file from Tasks 1-4

- [ ] **Step 1: Build all crates**

```bash
cargo build
```

Fix any compilation errors.

- [ ] **Step 2: Run all tests**

```bash
cargo test
```

Fix any test failures.

- [ ] **Step 3: Verify `carina init` still works**

```bash
# Test with a .crn that has source
cat > /tmp/test-phase4.crn << 'EOF'
provider awscc {
    source = "file:///path/to/carina-provider-awscc"
    version = "0.1.0"
    region = awscc.Region.ap_northeast_1
}
EOF
# This should not error (would fail at binary not found, but parsing/resolution logic is correct)
```

- [ ] **Step 4: Verify error message for source-less provider**

```bash
cat > /tmp/test-no-source.crn << 'EOF'
provider awscc {
    region = awscc.Region.ap_northeast_1
}
EOF
cargo run --bin carina -- validate /tmp/test-no-source.crn
```

Expected: Clear error message about missing `source` and `version`.

- [ ] **Step 5: Commit any fixes**

```bash
git add -A
git commit -m "fix: resolve integration issues after removing direct provider dependencies"
```

---

## Summary

| Task | Description | Key Output |
|------|-------------|------------|
| 1 | Dynamic WiringContext | `build_factories_from_providers()`, `WiringContext::new(factories)` |
| 2 | Remove provider deps | carina-cli no longer depends on aws/awscc providers |
| 3 | Fix tests | Updated fixtures and test code |
| 4 | LSP cleanup | Remove hardcoded factories from LSP |
| 5 | Validate | Full workspace build and test |

### Dependencies NOT removed (still needed for state backend / encryption)

- `aws-config` — AWS SDK configuration for S3 backend and KMS
- `aws-sdk-kms` — KMS decryption for sensitive values in `.crn`
- `aws-sdk-s3` — S3 state backend for remote state storage
