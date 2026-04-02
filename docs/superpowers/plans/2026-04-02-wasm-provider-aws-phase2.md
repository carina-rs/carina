# WASM Provider AWS — Phase 2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Compile carina-provider-aws to wasm32-wasip2 and verify it can make actual AWS API calls via wasi:http.

**Architecture:** The AWS provider compiles to a WASM component using `default-features = false` for AWS SDK crates (disabling the native HTTP client). A custom `WasiHttpClient` implements the AWS SDK's `HttpClient` trait, routing HTTP requests through `wasi:http/outgoing-handler`. The host (carina-cli via Wasmtime) provides the wasi:http implementation.

**Tech Stack:** AWS SDK for Rust (no-default-features), wasi:http, wit-bindgen, wasmtime-wasi-http

**Spec:** `docs/superpowers/specs/2026-03-31-wasm-provider-plugins-design.md`

**Repositories involved:**
- `carina-rs/carina` (monorepo): WasiHttpClient in carina-plugin-sdk, wasmtime-wasi-http in carina-plugin-host
- `carina-rs/carina-provider-aws`: WASM compilation of the AWS provider

---

## Prerequisite: WasiHttpClient (Monorepo)

### Task 1: Add wasmtime-wasi-http to carina-plugin-host

**Repo:** `carina-rs/carina`

**Files:**
- Modify: `carina-plugin-host/Cargo.toml`
- Modify: `carina-plugin-host/src/wasm_factory.rs`

The `carina-provider-with-http` WIT world imports `wasi:http/outgoing-handler`. The host must provide this import when instantiating providers that need HTTP.

- [ ] **Step 1: Add wasmtime-wasi-http dependency**

Add to `carina-plugin-host/Cargo.toml`:

```toml
wasmtime-wasi-http = "29"
```

- [ ] **Step 2: Add a second bindgen for the HTTP world**

In `carina-plugin-host/src/lib.rs`, add:

```rust
pub mod wasm_bindings_http {
    wasmtime::component::bindgen!({
        path: "../carina-plugin-wit/wit",
        world: "carina-provider-with-http",
    });
}
```

- [ ] **Step 3: Update WasmProviderFactory to support HTTP**

In `wasm_factory.rs`, add a `from_file_with_http()` constructor (or a flag) that additionally links `wasmtime_wasi_http` when creating instances:

```rust
impl WasmProviderFactory {
    pub fn from_file_with_http(wasm_path: &Path) -> Result<Self, String> {
        // Same as from_file but:
        // 1. Uses wasm_bindings_http::CarinaProviderWithHttp instead of CarinaProvider
        // 2. Adds wasmtime_wasi_http::add_only_http_to_linker_sync() to the linker
        // 3. HostState includes WasiHttpCtx
    }
}
```

Update `HostState`:

```rust
struct HostState {
    wasi_ctx: WasiCtx,
    http_ctx: wasmtime_wasi_http::WasiHttpCtx,
    table: ResourceTable,
}
```

Implement `wasmtime_wasi_http::WasiHttpView` for `HostState`.

- [ ] **Step 4: Verify it compiles**

```bash
cargo check -p carina-plugin-host
```

- [ ] **Step 5: Commit**

```bash
git add carina-plugin-host/
git commit -m "feat: add wasmtime-wasi-http support to WasmProviderFactory"
```

---

### Task 2: Implement WasiHttpClient in carina-plugin-sdk

**Repo:** `carina-rs/carina`

**Files:**
- Create: `carina-plugin-sdk/src/wasi_http.rs`
- Modify: `carina-plugin-sdk/Cargo.toml`
- Modify: `carina-plugin-sdk/src/lib.rs`

The WasiHttpClient implements the AWS SDK's `HttpClient` trait (from `aws-smithy-runtime-api`) and routes requests through `wasi:http/outgoing-handler`.

- [ ] **Step 1: Research AWS SDK HttpClient trait**

Check the `aws-smithy-runtime-api` crate for the exact trait signature:

```bash
# In a scratch project or via docs.rs
cargo doc -p aws-smithy-runtime-api --open
```

The trait is typically:

```rust
pub trait HttpClient: Send + Sync + Debug {
    fn http_connector(
        &self,
        settings: &HttpConnectorSettings,
        components: &RuntimeComponents,
    ) -> SharedHttpConnector;
}
```

Or it may use `HttpConnector` directly. The exact API depends on the aws-smithy version. Research this first.

- [ ] **Step 2: Add dependencies**

Add to `carina-plugin-sdk/Cargo.toml`:

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
aws-smithy-runtime-api = { version = "1", default-features = false }
aws-smithy-types = { version = "1", default-features = false }
```

- [ ] **Step 3: Create WasiHttpClient**

Create `carina-plugin-sdk/src/wasi_http.rs`:

```rust
//! WASM-compatible HTTP client using wasi:http/outgoing-handler.
//!
//! This client implements the AWS SDK's HttpConnector trait,
//! allowing AWS SDK operations to work in WASM environments
//! where wasi:http is provided by the host.

#[cfg(target_arch = "wasm32")]
// Implementation that:
// 1. Takes an HttpRequest from the AWS SDK
// 2. Converts it to a wasi:http OutgoingRequest
// 3. Calls wasi:http/outgoing-handler.handle()
// 4. Converts the IncomingResponse back to an HttpResponse
```

The wasi:http guest API is available via wit-bindgen (already added for the `carina-provider` world). For the `carina-provider-with-http` world, the generated bindings include `wasi::http::outgoing_handler::handle()`.

- [ ] **Step 4: Add conditional export**

In `carina-plugin-sdk/src/lib.rs`:

```rust
#[cfg(target_arch = "wasm32")]
pub mod wasi_http;
```

- [ ] **Step 5: Verify it compiles for native**

```bash
cargo check -p carina-plugin-sdk
```

(WASM compilation will be verified when building the provider)

- [ ] **Step 6: Commit**

```bash
git add carina-plugin-sdk/
git commit -m "feat: add WasiHttpClient for AWS SDK in WASM"
```

---

### Task 3: Update wasm_guest.rs for HTTP world

**Repo:** `carina-rs/carina`

**Files:**
- Modify: `carina-plugin-sdk/src/wasm_guest.rs`

The current `export_provider!` macro uses the `carina-provider` world (no HTTP). Providers that need HTTP must use the `carina-provider-with-http` world.

- [ ] **Step 1: Add a second macro for HTTP providers**

Add `export_provider_with_http!` macro that:
- Uses `wit_bindgen::generate!` with `world: "carina-provider-with-http"`
- Otherwise identical to `export_provider!`

Or, parameterize the existing macro:

```rust
#[macro_export]
macro_rules! export_provider {
    ($provider_type:ty) => {
        // Uses carina-provider world (no HTTP)
        $crate::__export_provider_impl!($provider_type, "carina-provider");
    };
    ($provider_type:ty, http) => {
        // Uses carina-provider-with-http world (with HTTP)
        $crate::__export_provider_impl!($provider_type, "carina-provider-with-http");
    };
}
```

- [ ] **Step 2: Verify MockProvider still works**

```bash
cargo build -p carina-provider-mock --target wasm32-wasip2
cargo test -p carina-plugin-host wasm_integration
```

- [ ] **Step 3: Commit**

```bash
git add carina-plugin-sdk/
git commit -m "feat: add HTTP world support to export_provider! macro"
```

---

## AWS Provider WASM Compilation

### Task 4: Add WASM dependencies and cfg-gates

**Repo:** `carina-rs/carina-provider-aws`

**Files:**
- Modify: `carina-provider-aws/Cargo.toml`

- [ ] **Step 1: Add cfg-gated dependencies for WASM**

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
wit-bindgen = { version = "0.51", default-features = false, features = ["macros"] }

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
# Keep existing deps that don't work on WASM with their current features
tokio = { version = "1", features = ["full"] }

[target.'cfg(target_arch = "wasm32")'.dependencies]
# WASM-compatible tokio (minimal features)
tokio = { version = "1", default-features = false, features = ["rt", "macros"] }
```

- [ ] **Step 2: Adjust AWS SDK features for WASM**

The AWS SDK crates need `default-features = false` on WASM to avoid pulling in hyper/socket2:

```toml
[dependencies]
# Shared deps (work on both targets)
aws-sdk-s3 = { version = "1", default-features = false, features = ["behavior-version-latest"] }
aws-sdk-ec2 = { version = "1", default-features = false, features = ["behavior-version-latest"] }
aws-sdk-iam = { version = "1", default-features = false, features = ["behavior-version-latest"] }
aws-sdk-cloudwatchlogs = { version = "1", default-features = false, features = ["behavior-version-latest"] }
aws-sdk-sts = { version = "1", default-features = false, features = ["behavior-version-latest"] }
aws-config = { version = "1", default-features = false, features = ["behavior-version-latest"] }

[target.'cfg(not(target_arch = "wasm32"))'.dependencies]
# Native: re-enable default features for HTTP client
aws-sdk-s3 = { version = "1", features = ["behavior-version-latest"] }
# ... same for other AWS SDK crates
```

Note: The exact Cargo.toml syntax for conditional default-features may require a different approach. Test what works. The goal: on native, use full AWS SDK with hyper; on WASM, use SDK types only + custom HTTP client.

- [ ] **Step 3: Try compiling for native (no regressions)**

```bash
cargo check
```

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml carina-provider-aws/Cargo.toml
git commit -m "feat: add WASM-compatible dependency configuration"
```

---

### Task 5: Swap HTTP client for WASM

**Repo:** `carina-rs/carina-provider-aws`

**Files:**
- Modify: `carina-provider-aws/src/lib.rs`

- [ ] **Step 1: Add cfg-gated AWS config builder**

```rust
impl AwsProvider {
    pub async fn new(region: &str) -> Self {
        let config = Self::build_config(region).await;
        Self {
            s3_client: S3Client::new(&config),
            ec2_client: Ec2Client::new(&config),
            iam_client: IamClient::new(&config),
            logs_client: CloudWatchLogsClient::new(&config),
            sts_client: StsClient::new(&config),
            region: region.to_string(),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn build_config(region: &str) -> SdkConfig {
        aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .load()
            .await
    }

    #[cfg(target_arch = "wasm32")]
    async fn build_config(region: &str) -> SdkConfig {
        let http_client = carina_plugin_sdk::wasi_http::WasiHttpClient::new();
        aws_config::defaults(BehaviorVersion::latest())
            .region(Region::new(region.to_string()))
            .http_client(http_client)
            .load()
            .await
    }
}
```

- [ ] **Step 2: Handle tokio runtime for WASM**

The current `AwsProcessProvider` creates a `tokio::runtime::Runtime`. On WASM, the current_thread runtime should work:

```rust
impl AwsProcessProvider {
    fn new() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        let runtime = tokio::runtime::Runtime::new().unwrap();
        #[cfg(target_arch = "wasm32")]
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        
        Self {
            runtime,
            provider: None,
            normalizer: AwsNormalizer,
        }
    }
}
```

- [ ] **Step 3: Verify native still compiles**

```bash
cargo check
```

- [ ] **Step 4: Commit**

```bash
git add carina-provider-aws/src/lib.rs carina-provider-aws/src/main.rs
git commit -m "feat: add cfg-gated HTTP client swap for WASM"
```

---

### Task 6: Add WASM entry point

**Repo:** `carina-rs/carina-provider-aws`

**Files:**
- Modify: `carina-provider-aws/src/main.rs`

- [ ] **Step 1: Add WASM entry point**

```rust
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    carina_plugin_sdk::run(AwsProcessProvider::new());
}

#[cfg(target_arch = "wasm32")]
carina_plugin_sdk::export_provider!(AwsProcessProvider, http);
```

The `http` flag tells `export_provider!` to use the `carina-provider-with-http` world.

- [ ] **Step 2: Verify native still compiles and tests pass**

```bash
cargo test
```

- [ ] **Step 3: Commit**

```bash
git add carina-provider-aws/src/main.rs
git commit -m "feat: add WASM entry point with HTTP world"
```

---

### Task 7: Compile to wasm32-wasip2

**Repo:** `carina-rs/carina-provider-aws`

- [ ] **Step 1: Try compiling**

```bash
cargo build -p carina-provider-aws --target wasm32-wasip2
```

- [ ] **Step 2: Fix compilation errors**

Expected issues and solutions:

**a) AWS SDK crypto (aws-lc-rs / ring):**
- Try `aws-config = { features = ["rustls"] }` or investigate `aws-lc-rs` WASM support
- If crypto crate fails, try `default-features = false` and manually select features

**b) tokio features:**
- Only `rt`, `macros`, `sync`, `time` are expected to work on WASM
- `net`, `fs`, `signal` won't work — ensure they're not pulled in

**c) hyper/socket2:**
- Should be excluded by `default-features = false` on AWS SDK crates
- If still pulled in, trace the dependency tree: `cargo tree -e features -i socket2 --target wasm32-wasip2`

**d) System calls (libc, etc.):**
- Some crates may use OS-specific APIs — check error messages

- [ ] **Step 3: Iterate until compilation succeeds**

Document every change needed in a WASM_BUILD_NOTES.md file.

- [ ] **Step 4: Check binary size**

```bash
ls -la target/wasm32-wasip2/debug/carina_provider_aws.wasm
ls -la target/wasm32-wasip2/release/carina_provider_aws.wasm
```

- [ ] **Step 5: Commit**

```bash
git add .
git commit -m "feat: compile carina-provider-aws to wasm32-wasip2"
```

---

### Task 8: E2E Test — WASM provider plan/apply

**Repos:** Both `carina-rs/carina` and `carina-rs/carina-provider-aws`

- [ ] **Step 1: Test with Wasmtime CLI (basic smoke test)**

```bash
aws-vault exec mizzy -- wasmtime run \
    --wasi http \
    target/wasm32-wasip2/debug/carina_provider_aws.wasm
```

This should start the provider and wait for input (or crash with a useful error).

- [ ] **Step 2: Test via carina CLI**

In the monorepo, create a test .crn file that uses the WASM provider:

```
provider "aws" {
    source = "file:///path/to/carina_provider_aws.wasm"
    region = "ap-northeast-1"
}

aws.sts.caller_identity {}
```

Run:

```bash
aws-vault exec mizzy -- cargo run -- plan .
```

The `file://` source with `.wasm` extension should trigger WasmProviderFactory.

- [ ] **Step 3: Test a real resource**

Create a test with S3 bucket:

```
provider "aws" {
    source = "file:///path/to/carina_provider_aws.wasm"
    region = "ap-northeast-1"
}

aws.s3.bucket {
    bucket = "carina-wasm-test-bucket"
}
```

Run plan and apply to verify actual AWS API calls work through wasi:http.

- [ ] **Step 4: Document results**

Create `WASM_TEST_RESULTS.md` documenting what works and what doesn't.

- [ ] **Step 5: Clean up test resources**

Delete any AWS resources created during testing.

---

### Task 9: Update CI for WASM builds

**Repo:** `carina-rs/carina-provider-aws`

**Files:**
- Modify: `.github/workflows/ci.yml`
- Modify: `.github/workflows/release.yml`

- [ ] **Step 1: Add WASM build to CI**

In `.github/workflows/ci.yml`, add a job:

```yaml
wasm-build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-wasip2
      - run: cargo build -p carina-provider-aws --target wasm32-wasip2 --release
```

- [ ] **Step 2: Add WASM artifact to release workflow**

In `.github/workflows/release.yml`, add a WASM build job that:
1. Builds `cargo build --target wasm32-wasip2 --release`
2. Creates `carina-provider-aws.wasm` artifact
3. Uploads to GitHub release alongside native binaries

- [ ] **Step 3: Commit**

```bash
git add .github/
git commit -m "ci: add WASM build to CI and release workflows"
```

---

## Phase 2 Complete When

- [ ] carina-provider-aws compiles to wasm32-wasip2
- [ ] Native compilation and tests still pass (no regressions)
- [ ] `sts.caller_identity` works via WASM provider (basic smoke test)
- [ ] At least one mutable resource (e.g., S3 bucket) works via WASM provider
- [ ] CI includes WASM build
- [ ] Release workflow produces .wasm artifact
