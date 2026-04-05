# IMDS Probe Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Auto-detect metadata environments (EC2/ECS) at plugin startup and disable IMDS for non-metadata environments so the AWS SDK doesn't hang on retries.

**Architecture:** Host-side TCP probe of metadata endpoints before WASM plugin initialization. Result cached in `OnceLock`. Probe failure sets `AWS_EC2_METADATA_DISABLED=true` in the plugin's WASI environment. ECS container credential endpoint also added to HTTP allow-list.

**Tech Stack:** `std::net::TcpStream`, `std::sync::OnceLock`, `std::thread::scope`

**Spec:** `docs/superpowers/specs/2026-04-05-imds-probe-design.md`

**Working branch:** `fix/issue-1527-imds-timeout` (worktree at `.worktrees/fix/issue-1527-imds-timeout`)

---

### Task 1: Add ECS endpoint to HTTP allow-list and env allowlist

**Files:**
- Modify: `carina-plugin-host/src/wasm_factory.rs:41-42` (IMDS_HOST constant area)
- Modify: `carina-plugin-host/src/wasm_factory.rs:54-62` (is_host_allowed, is_imds_host)
- Modify: `carina-plugin-host/src/wasm_factory.rs:436-446` (WASM_ENV_ALLOWLIST)
- Modify: `carina-plugin-host/src/wasm_factory.rs:96-107` (timeout cap in send_request)

- [ ] **Step 1: Write failing tests for ECS endpoint**

Add to the `tests` module at the end of `wasm_factory.rs`:

```rust
#[test]
fn test_http_allowlist_permits_ecs_metadata() {
    assert!(is_host_allowed("169.254.170.2"));
    assert!(is_host_allowed("169.254.170.2:80"));
}

#[test]
fn test_is_metadata_host() {
    // IMDS
    assert!(is_metadata_host("169.254.169.254"));
    assert!(is_metadata_host("169.254.169.254:80"));
    // ECS
    assert!(is_metadata_host("169.254.170.2"));
    assert!(is_metadata_host("169.254.170.2:80"));
    // Not metadata
    assert!(!is_metadata_host("s3.amazonaws.com"));
    assert!(!is_metadata_host("169.254.170.3"));
}

#[test]
fn test_ecs_env_vars_in_allowlist() {
    assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
    assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_FULL_URI"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p carina-plugin-host test_http_allowlist_permits_ecs test_is_metadata_host test_ecs_env_vars`
Expected: FAIL — `is_host_allowed("169.254.170.2")` returns false, `is_metadata_host` doesn't exist, ECS env vars not in allowlist.

- [ ] **Step 3: Rename IMDS_HOST to METADATA_HOSTS array and update functions**

Replace the `IMDS_HOST` constant and `is_host_allowed`/`is_imds_host` functions:

```rust
/// Metadata endpoint addresses (EC2 IMDS and ECS task metadata).
const METADATA_HOSTS: &[&str] = &["169.254.169.254", "169.254.170.2"];

/// ...existing IMDS_CONNECT_TIMEOUT stays the same...

/// Strip port from authority (e.g., "s3.amazonaws.com:443" -> "s3.amazonaws.com").
fn host_without_port(host: &str) -> &str {
    host.split(':').next().unwrap_or(host)
}

/// Returns `true` if the given host (authority without port) is allowed
/// by the HTTP allow-list.
fn is_host_allowed(host: &str) -> bool {
    let h = host_without_port(host);
    METADATA_HOSTS.contains(&h)
        || HTTP_ALLOWED_HOST_SUFFIXES
            .iter()
            .any(|suffix| h.ends_with(suffix))
}

/// Returns `true` if the host is a metadata endpoint (IMDS or ECS).
fn is_metadata_host(host: &str) -> bool {
    METADATA_HOSTS.contains(&host_without_port(host))
}
```

Update the `AllowListHttpHooks` doc comment to reference `METADATA_HOSTS` instead of `IMDS_HOST`. Update the timeout cap block in `send_request` to call `is_metadata_host` instead of `is_imds_host`:

```rust
if is_metadata_host(authority) {
```

- [ ] **Step 4: Add ECS env vars to WASM_ENV_ALLOWLIST**

```rust
const WASM_ENV_ALLOWLIST: &[&str] = &[
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "AWS_REGION",
    "AWS_DEFAULT_REGION",
    "AWS_ENDPOINT_URL",
    "AWS_EC2_METADATA_DISABLED",
    "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
    "AWS_CONTAINER_CREDENTIALS_FULL_URI",
    "HOME",
    "RUST_LOG",
];
```

- [ ] **Step 5: Update existing tests that reference is_imds_host**

Replace the `test_is_imds_host` test:

```rust
#[test]
fn test_is_imds_host() {
    // Replaced by test_is_metadata_host
}
```

Actually, just delete `test_is_imds_host` entirely and update `test_imds_connect_timeout_is_short` — it can stay as-is since the constant name hasn't changed.

Update the `test_wasm_env_allowlist_contains_required_vars` test to also assert the new ECS vars:

```rust
assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_RELATIVE_URI"));
assert!(WASM_ENV_ALLOWLIST.contains(&"AWS_CONTAINER_CREDENTIALS_FULL_URI"));
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p carina-plugin-host`
Expected: All tests pass.

- [ ] **Step 7: Run clippy and fmt**

Run: `cargo clippy -p carina-plugin-host && cargo fmt -p carina-plugin-host --check`
Expected: No warnings or errors.

- [ ] **Step 8: Commit**

```bash
git add carina-plugin-host/src/wasm_factory.rs
git commit -m "feat: add ECS metadata endpoint to HTTP allow-list and env allowlist"
```

---

### Task 2: Add metadata probe function

**Files:**
- Modify: `carina-plugin-host/src/wasm_factory.rs` (add `is_metadata_available` after `is_metadata_host`)

- [ ] **Step 1: Write failing test for probe**

Add to the `tests` module:

```rust
#[test]
fn test_metadata_probe_returns_false_on_unreachable() {
    // On non-EC2/ECS environments (CI, local dev), the probe should return false.
    // This test assumes it's not running on EC2 or ECS.
    // The probe targets link-local addresses that are unreachable off-instance.
    let start = std::time::Instant::now();
    let result = probe_metadata_endpoints();
    let elapsed = start.elapsed();
    assert!(!result, "probe should return false on non-metadata environment");
    // Should complete within 2 seconds (1s timeout + margin)
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "probe took too long: {elapsed:?}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p carina-plugin-host test_metadata_probe`
Expected: FAIL — `probe_metadata_endpoints` doesn't exist.

- [ ] **Step 3: Implement probe function**

Add after `is_metadata_host`:

```rust
/// Probe metadata endpoints and return true if any is reachable.
///
/// Uses parallel TCP connect attempts with a 1-second timeout.
/// Called once at startup; result is cached by the caller.
fn probe_metadata_endpoints() -> bool {
    use std::net::{SocketAddr, TcpStream};

    std::thread::scope(|s| {
        let handles: Vec<_> = METADATA_HOSTS
            .iter()
            .map(|host| {
                s.spawn(move || {
                    let addr: SocketAddr = format!("{host}:80").parse().unwrap();
                    TcpStream::connect_timeout(&addr, METADATA_PROBE_TIMEOUT).is_ok()
                })
            })
            .collect();
        handles.into_iter().any(|h| h.join().unwrap_or(false))
    })
}
```

Add the timeout constant (rename `IMDS_CONNECT_TIMEOUT` to `METADATA_PROBE_TIMEOUT` since it now serves both the probe and the request timeout cap):

Replace:
```rust
const IMDS_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
```
With:
```rust
/// Timeout for metadata endpoint connections. On EC2/ECS, metadata responds
/// in <10ms. 1 second is generous and lets non-metadata environments fail fast.
const METADATA_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
```

Update all references from `IMDS_CONNECT_TIMEOUT` to `METADATA_PROBE_TIMEOUT` in the `send_request` method and tests.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p carina-plugin-host`
Expected: All tests pass. The probe test should complete in ~1 second.

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy -p carina-plugin-host && cargo fmt -p carina-plugin-host --check`
Expected: No warnings or errors.

- [ ] **Step 6: Commit**

```bash
git add carina-plugin-host/src/wasm_factory.rs
git commit -m "feat: add metadata endpoint probe function"
```

---

### Task 3: Integrate probe with WASI context and add caching

**Files:**
- Modify: `carina-plugin-host/src/wasm_factory.rs:448-458` (build_sandboxed_wasi_ctx)

- [ ] **Step 1: Write failing test for WASI context integration**

Add to the `tests` module:

```rust
#[test]
fn test_metadata_probe_result_is_cached() {
    // Calling is_metadata_available() twice should return the same result
    // and the second call should be near-instant (cached).
    let first = is_metadata_available();
    let start = std::time::Instant::now();
    let second = is_metadata_available();
    let elapsed = start.elapsed();
    assert_eq!(first, second);
    assert!(
        elapsed < std::time::Duration::from_millis(10),
        "second call should be cached, took {elapsed:?}"
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p carina-plugin-host test_metadata_probe_result_is_cached`
Expected: FAIL — `is_metadata_available` doesn't exist.

- [ ] **Step 3: Implement is_metadata_available with OnceLock caching**

Add after `probe_metadata_endpoints`:

```rust
/// Returns `true` if any metadata endpoint is reachable.
/// Result is cached for the lifetime of the process.
fn is_metadata_available() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(probe_metadata_endpoints)
}
```

Add the import at the top of the file (in the existing use block):

```rust
use std::sync::OnceLock;
```

- [ ] **Step 4: Modify build_sandboxed_wasi_ctx to use probe result**

```rust
fn build_sandboxed_wasi_ctx() -> WasiCtx {
    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stderr();
    for key in WASM_ENV_ALLOWLIST {
        if let Ok(val) = std::env::var(key) {
            builder.env(key, &val);
        }
    }
    // Auto-disable IMDS if metadata endpoints are unreachable,
    // unless the user has explicitly set the variable.
    if std::env::var("AWS_EC2_METADATA_DISABLED").is_err() && !is_metadata_available() {
        builder.env("AWS_EC2_METADATA_DISABLED", "true");
    }
    builder.build()
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p carina-plugin-host`
Expected: All tests pass.

- [ ] **Step 6: Run clippy and fmt**

Run: `cargo clippy -p carina-plugin-host && cargo fmt -p carina-plugin-host --check`
Expected: No warnings or errors.

- [ ] **Step 7: Commit**

```bash
git add carina-plugin-host/src/wasm_factory.rs
git commit -m "feat: auto-disable IMDS when metadata endpoints are unreachable"
```

---

### Task 4: Push and create PR

- [ ] **Step 1: Run full test suite**

Run: `cargo test -p carina-plugin-host`
Expected: All tests pass.

- [ ] **Step 2: Push and create PR**

```bash
git push
gh pr create --title "fix: auto-disable IMDS on non-metadata environments via host probe" \
  --body "$(cat <<'PREOF'
## Summary

- Probe EC2 IMDS (169.254.169.254) and ECS metadata (169.254.170.2) at plugin startup
- If both are unreachable (1s timeout), auto-set AWS_EC2_METADATA_DISABLED=true in the WASM plugin environment
- Add ECS container credential endpoint to HTTP allow-list and env allowlist
- Rename IMDS-specific constants to metadata-generic names

## Context

PR #1530 added IMDS to the allow-list. PR #1536 capped timeouts. But SDK retry backoff still causes 30s+ hangs on non-EC2. This PR probes metadata endpoints once at startup and disables IMDS if unreachable, so the SDK skips it entirely.

## Behavior

| Environment | Probe result | SDK behavior |
|-------------|-------------|--------------|
| EC2 | 169.254.169.254 responds | IMDS enabled |
| ECS | 169.254.170.2 responds | IMDS enabled |
| Local/CI | Both timeout (1s) | IMDS disabled |
| Any + AWS_EC2_METADATA_DISABLED=true | Probe skipped | IMDS disabled |
| Any + AWS_EC2_METADATA_DISABLED=false | Probe skipped | IMDS enabled |

## Test plan

- [x] Probe returns false on non-metadata environment within ~1s
- [x] Probe result is cached (second call is instant)
- [x] ECS endpoint permitted by HTTP allow-list
- [x] ECS env vars in WASM env allowlist
- [x] All existing tests pass

closes #1527

🤖 Generated with [Claude Code](https://claude.com/claude-code)
PREOF
)"
```

Note: This PR supersedes PR #1536. After merge, close #1536 if still open.
