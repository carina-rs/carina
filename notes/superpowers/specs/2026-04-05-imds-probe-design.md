# IMDS Probe Design

## Problem

PR #1530 added IMDS (`169.254.169.254`) to the WASM plugin HTTP allow-list, fixing repeated "blocked" warnings. However, on non-EC2 environments, the AWS SDK retries IMDS requests with exponential backoff, causing `apply`/`destroy` to hang for 30+ seconds before failing.

Capping connect/first_byte timeouts (PR #1536) reduces individual request time to 1 second, but SDK-internal retry backoff still dominates total wait time.

## Solution

Probe metadata endpoints from the host (CLI process) at plugin startup. If unreachable, set `AWS_EC2_METADATA_DISABLED=true` in the WASM plugin's environment so the SDK skips IMDS entirely.

## Probe Targets

| Endpoint | Port | Purpose |
|----------|------|---------|
| `169.254.169.254` | 80 | EC2 Instance Metadata Service (IMDS) |
| `169.254.170.2` | 80 | ECS task metadata / container credentials |

If either endpoint responds, the environment is considered "metadata-capable" and IMDS remains enabled. If both are unreachable, IMDS is disabled.

## Implementation

### Probe function

Location: `carina-plugin-host/src/wasm_factory.rs`

```rust
use std::net::{TcpStream, SocketAddr};
use std::sync::OnceLock;
use std::time::Duration;

const METADATA_ENDPOINTS: &[&str] = &["169.254.169.254:80", "169.254.170.2:80"];
const METADATA_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Returns `true` if any metadata endpoint is reachable.
/// Result is cached for the lifetime of the process.
fn is_metadata_available() -> bool {
    static RESULT: OnceLock<bool> = OnceLock::new();
    *RESULT.get_or_init(|| {
        std::thread::scope(|s| {
            let handles: Vec<_> = METADATA_ENDPOINTS
                .iter()
                .map(|ep| {
                    s.spawn(|| {
                        let addr: SocketAddr = ep.parse().unwrap();
                        TcpStream::connect_timeout(&addr, METADATA_PROBE_TIMEOUT).is_ok()
                    })
                })
                .collect();
            handles.into_iter().any(|h| h.join().unwrap_or(false))
        })
    })
}
```

Key points:
- Uses `std::thread::scope` to probe both endpoints in parallel (max 1 second wall time).
- `OnceLock` caches the result so multiple plugin loads don't re-probe.
- Synchronous API (`TcpStream::connect_timeout`) is appropriate since this runs once at startup.

### Integration with WASI context

Modify `build_sandboxed_wasi_ctx()` to conditionally set `AWS_EC2_METADATA_DISABLED`:

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

The user can override by setting `AWS_EC2_METADATA_DISABLED` on the host:
- `=true`: skip probe, always disable IMDS
- `=false`: skip probe, always enable IMDS

### Existing code retained

- **Timeout cap** (PR #1536): kept as defense-in-depth for when IMDS is enabled but slow.
- **HTTP allow-list** (PR #1530): kept so IMDS requests are permitted when the probe succeeds.
- **`AWS_EC2_METADATA_DISABLED` in `WASM_ENV_ALLOWLIST`** (PR #1536): kept for user override.

### ECS container credentials

ECS tasks get credentials via `169.254.170.2` (referenced by `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI`). The `169.254.170.2` endpoint is NOT in the HTTP allow-list, so it would be blocked by `AllowListHttpHooks`. To support ECS:

- Add `169.254.170.2` to the HTTP allow-list (same as IMDS).
- Apply the same timeout cap as IMDS.
- Add `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` and `AWS_CONTAINER_CREDENTIALS_FULL_URI` to `WASM_ENV_ALLOWLIST` (the SDK needs these to locate the ECS credential endpoint).

## Tests

1. **Probe timeout**: Call `is_metadata_available()` with unreachable addresses, verify it returns `false` within ~1 second.
2. **WASI context without metadata**: When `is_metadata_available()` returns `false` and `AWS_EC2_METADATA_DISABLED` is unset, verify the context includes `AWS_EC2_METADATA_DISABLED=true`.
3. **User override**: When `AWS_EC2_METADATA_DISABLED=false` is set on the host, verify the probe is not consulted and the variable is passed through as-is.
4. **ECS env vars**: Verify `AWS_CONTAINER_CREDENTIALS_RELATIVE_URI` and `AWS_CONTAINER_CREDENTIALS_FULL_URI` are in the allowlist.

## Behavior Summary

| Environment | Probe result | SDK behavior |
|-------------|-------------|--------------|
| EC2 | `169.254.169.254` responds | IMDS enabled, credentials from instance profile |
| ECS | `169.254.170.2` responds | IMDS enabled, credentials from task role |
| Local/CI (no metadata) | Both timeout (1s) | IMDS disabled, SDK uses env vars / config files |
| Any + `AWS_EC2_METADATA_DISABLED=true` | Probe skipped | IMDS disabled |
| Any + `AWS_EC2_METADATA_DISABLED=false` | Probe skipped | IMDS enabled |
