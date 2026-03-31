# WASM Provider Plugins Design

## Overview

Provider plugins を WebAssembly (WASM) Component Model に移行し、現在のプロセスベースプラグインシステム（JSON-RPC over stdin/stdout）を置き換える。

## Motivation

1. **クロスプラットフォーム配布の簡素化**（最優先）: OS/arch 別のネイティブバイナリ（6+ファイル）から単一 `.wasm` ファイルへ
2. **セキュリティ**: WASI sandbox によるファイルシステム・ネットワークアクセスの制限
3. **起動コスト削減**: プロセス起動 → WASM インスタンス化（プリコンパイルキャッシュ併用）

## Architecture

### Current (Process-Based)

```
carina-cli
  └── ProcessProviderFactory (carina-plugin-host)
        └── spawn subprocess → JSON-RPC over stdin/stdout → provider binary (OS/arch別)
```

### Target (WASM Component Model)

```
carina-cli
  └── WasmProviderFactory (carina-plugin-host)
        └── Wasmtime runtime
              └── WASM Component (.wasm)
                    ├── CarinaProvider WIT interface (read/create/update/delete/normalize/schemas)
                    └── wasi:http/outgoing-handler (AWS API呼び出し用)
```

### Key Changes

| Layer | Current | After WASM |
|---|---|---|
| Plugin format | OS/arch native binary | Single `.wasm` component |
| Protocol | JSON-RPC over stdin/stdout | WIT (Component Model interface) |
| Network I/O | Direct HTTP in provider | Via `wasi:http/outgoing-handler` |
| Process model | Child process | In-process (Wasmtime) |
| Type conversion | Core ↔ Protocol (JSON) | Core ↔ WIT types |

### Affected Crates

- **carina-plugin-host**: Add `WasmProviderFactory` alongside `ProcessProviderFactory`
- **carina-plugin-sdk**: Add WASM guest-side library (WIT bindings generation)
- **carina-provider-protocol**: Gradually replaced by WIT definitions (eventually removed)
- **carina-provider-aws / awscc**: Compile target change + HTTP adapter swap
- **carina-cli**: provider_resolver updated for `.wasm` download/cache

### Coexistence Strategy

During migration, `ProcessProviderFactory` and `WasmProviderFactory` coexist. File extension of the resolved provider artifact (binary vs `.wasm`) determines which factory is used.

## WIT Interface Definition

Package: `carina:provider@0.1.0`

### Types

```wit
interface types {
    record resource-id {
        provider: string,
        resource-type: string,
        name: string,
    }

    variant value {
        bool(bool),
        int(s64),
        float(f64),
        str(string),
        list(list<value>),
        map(list<tuple<string, value>>),
    }

    record state {
        identifier: option<string>,
        attributes: list<tuple<string, value>>,
    }

    record resource {
        id: resource-id,
        attributes: list<tuple<string, value>>,
    }

    record lifecycle-config {
        prevent-destroy: bool,
    }

    record provider-error {
        message: string,
        resource-id: option<resource-id>,
        is-timeout: bool,
    }

    record resource-schema {
        resource-type: string,
        attributes: list<attribute-schema>,
    }

    record attribute-schema {
        name: string,
        attr-type: attribute-type,
        required: bool,
        description: string,
        force-new: bool,
    }

    variant attribute-type {
        string-type,
        integer-type,
        float-type,
        boolean-type,
        list-type(attribute-type),
        map-type,
        enum-type(list<string>),
        custom-type(string),
        struct-type(struct-def),
    }

    record struct-def {
        name: string,
        fields: list<struct-field>,
    }

    record struct-field {
        name: string,
        field-type: attribute-type,
        required: bool,
        description: string,
    }
}
```

### Provider Interface

```wit
interface provider {
    use types.{resource-id, state, resource, lifecycle-config, provider-error, resource-schema, value};

    name: func() -> string;
    schemas: func() -> list<resource-schema>;
    validate-config: func(attrs: list<tuple<string, value>>) -> result<_, provider-error>;
    initialize: func(attrs: list<tuple<string, value>>) -> result<_, provider-error>;

    read: func(id: resource-id, identifier: option<string>) -> result<state, provider-error>;
    create: func(res: resource) -> result<state, provider-error>;
    update: func(id: resource-id, identifier: string, from: state, to: resource) -> result<state, provider-error>;
    delete: func(id: resource-id, identifier: string, lifecycle: lifecycle-config) -> result<_, provider-error>;

    normalize-desired: func(resources: list<resource>) -> list<resource>;
    normalize-state: func(states: list<tuple<resource-id, state>>) -> list<tuple<resource-id, state>>;
}

world carina-provider {
    import wasi:http/outgoing-handler@0.2.0;
    export provider;
}
```

### Design Decisions

- **`hydrate_read_state` and `merge_default_tags` omitted from initial WIT.** Used by AWS/AWSCC providers but can be added incrementally. Initial focus is CRUD + normalize.
- **CRUD functions are synchronous.** WASM Component Model async is not yet stable. Host wraps calls with `tokio::task::spawn_blocking` or similar.
- **Maps represented as `list<tuple<string, value>>`.** WIT has no HashMap type.

## Host-Side Implementation

### WasmProviderFactory

```rust
// carina-plugin-host/src/wasm_factory.rs

pub struct WasmProviderFactory {
    engine: wasmtime::Engine,
    component: wasmtime::component::Component,
    schemas: Vec<ResourceSchema>,
}

pub struct WasmProvider {
    store: Mutex<wasmtime::Store<HostState>>,
    instance: ProviderBindings,
}

struct HostState {
    wasi_ctx: WasiCtx,
    http_ctx: WasiHttpCtx,
    table: ResourceTable,
}
```

### Wasmtime Configuration

```rust
let mut config = wasmtime::Config::new();
config.wasm_component_model(true);
config.async_support(true);

let engine = Engine::new(&config)?;
let component = Component::from_file(&engine, &wasm_path)?;
```

### wasi:http Provision

Uses `wasmtime-wasi-http` crate. No custom proxy server needed — Wasmtime provides the standard `wasi:http` implementation backed by `hyper`.

```rust
let mut linker = Linker::new(&engine);
wasmtime_wasi::add_to_linker_async(&mut linker)?;
wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)?;
```

### WASI Capability Control (Sandbox)

```rust
let wasi_ctx = WasiCtxBuilder::new()
    // No filesystem access (no preopened dirs)
    // Only AWS credential env vars
    .env("AWS_ACCESS_KEY_ID", &access_key)
    .env("AWS_SECRET_ACCESS_KEY", &secret_key)
    .env("AWS_SESSION_TOKEN", &session_token)
    .env("AWS_REGION", &region)
    .inherit_stderr()
    .build();
```

AWS credentials are obtained on the host side and explicitly passed to the WASM environment. The WASM component cannot access `~/.aws/credentials` directly.

### Type Conversion

`carina-plugin-host/src/wasm_convert.rs` handles Core types ↔ WIT types conversion. Same role as existing `convert.rs` (Core ↔ Protocol JSON) but without JSON serialization overhead.

### Dependencies

```toml
# carina-plugin-host/Cargo.toml
wasmtime = { version = "...", features = ["component-model"] }
wasmtime-wasi = "..."
wasmtime-wasi-http = "..."
```

## Provider-Side Implementation

### Compile Target

```bash
cargo build --target wasm32-wasip2 -p carina-provider-aws --release
# Output: target/wasm32-wasip2/release/carina_provider_aws.wasm
```

### SDK Changes

Provider authors continue implementing `CarinaProvider` trait. The SDK provides a WASM export macro:

```rust
// carina-plugin-sdk

#[cfg(not(target_arch = "wasm32"))]
pub fn run(provider: impl CarinaProvider) { /* JSON-RPC loop */ }

#[cfg(target_arch = "wasm32")]
#[macro_export]
macro_rules! export_provider {
    ($provider_type:ty) => {
        // Bridges CarinaProvider trait methods to WIT exports
    };
}
```

Provider usage:

```rust
struct AwsProvider { /* ... */ }
impl CarinaProvider for AwsProvider { /* existing code, mostly unchanged */ }
carina_plugin_sdk::export_provider!(AwsProvider);
```

### AWS SDK HTTP Adapter Swap

```rust
#[cfg(target_arch = "wasm32")]
fn build_aws_config(region: &str) -> SdkConfig {
    let http_client = WasiHttpClient::new();
    aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .http_client(http_client)
        .load()
        .await
}

#[cfg(not(target_arch = "wasm32"))]
fn build_aws_config(region: &str) -> SdkConfig {
    aws_config::defaults(BehaviorVersion::latest())
        .region(Region::new(region.to_string()))
        .load()
        .await
}
```

`WasiHttpClient` implements AWS SDK's `HttpClient` trait, internally calling `wasi:http/outgoing-handler`. AWS SDK code (`s3_client.get_object()` etc.) requires no changes.

## Plugin Distribution and Loading

### Distribution Format

| Item | Current | After WASM |
|---|---|---|
| Distribution unit | OS/arch binaries (`...-darwin-arm64`, `...-linux-amd64`) | Single `.wasm` file |
| GitHub Release assets | 6+ files | 1 file + SHA256 |

### Cache Structure

```
~/.carina/providers/
  └── github.com/carina-rs/carina-provider-aws/
      └── v0.5.0/
          ├── carina-provider-aws.wasm      # Original WASM
          └── carina-provider-aws.cwasm     # Precompiled (Wasmtime version-specific)
```

### Precompile (AOT) Cache

```rust
// First load
let component = Component::from_file(&engine, &wasm_path)?;
let serialized = engine.precompile_component(&wasm_bytes)?;
std::fs::write(&cache_path, &serialized)?;

// Subsequent loads (fast)
let component = unsafe { Component::deserialize_file(&engine, &cache_path)? };
```

`.cwasm` is Wasmtime version-specific. Cache key includes Wasmtime version; regenerated on version change.

### Loading Flow

1. Extract `source` from provider config
2. Check cache for `.wasm` → download from GitHub Release + SHA256 verify if missing
3. Check precompile cache (`.cwasm`) → precompile and cache if missing
4. `Component::deserialize_file()` for fast load
5. Create `WasmProviderFactory`
6. Cache schemas on first access

### Coexistence with Native Binaries

```rust
fn create_factory(resolved_path: &Path, ...) -> Box<dyn ProviderFactory> {
    if resolved_path.extension() == Some("wasm") {
        Box::new(WasmProviderFactory::new(engine, resolved_path)?)
    } else {
        Box::new(ProcessProviderFactory::new(resolved_path))
    }
}
```

## Migration Strategy

### Phases

| Phase | Description | Done When |
|---|---|---|
| **Phase 0: PoC** | Verify AWS SDK compiles to wasm32-wasip2 | S3 read works from WASM |
| **Phase 1: Foundation** | WIT definition, WasmProviderFactory, SDK WASM support | MockProvider works as WASM |
| **Phase 2: AWS Provider** | Compile carina-provider-aws to WASM | All tests pass, plan/apply works |
| **Phase 3: AWSCC Provider** | Compile carina-provider-awscc to WASM | All tests pass |
| **Phase 4: Distribution** | Switch GitHub Releases to .wasm, precompile cache | Users can use .wasm providers |
| **Phase 5: Cleanup** | Remove ProcessProviderFactory, carina-provider-protocol | Code reduction |

### Phase 0 (PoC) Details

Verify the highest-risk item first: AWS SDK wasm32-wasip2 compilation and runtime behavior.

Verification items:
1. `aws-sdk-s3` compiles to `wasm32-wasip2`
2. Crypto library (`aws-lc-rs` / `ring`) works in WASM
3. `WasiHttpClient` can call S3 ListBuckets
4. SigV4 signing works correctly (including time retrieval)

Method: Minimal standalone binary compiled to wasm32-wasip2, run with Wasmtime.

### Fallback Options (If AWS SDK Doesn't Compile)

**Option F1: Wait for upstream WASM support.** Identify specific blockers, file issues/PRs upstream. AWS SDK Rust WASM support is progressing.

**Option F2: Lightweight HTTP client.** Skip AWS SDK in WASM, call AWS APIs directly with a custom HTTP client. Use `aws-sigv4` for signing (if it compiles to WASM). AWSCC provider (CloudControl API / REST) is least affected.

**Option F3: Fall back to Approach C.** Only schema/normalization in WASM, AWS API calls on host side. Most reliable but largest architectural change.

### Fallback Decision Flow

```
Phase 0 PoC
  ├── Compiles & works → Proceed to Phase 1
  ├── Compiles with partial issues → Identify issues, try F1 or F2
  └── Cannot compile → Try F2 → If F2 fails → F3
```

## Testing Strategy

### Phase 1 (Foundation)

```rust
// carina-plugin-host/tests/wasm_provider_test.rs
#[tokio::test]
async fn test_wasm_mock_provider_crud() {
    let factory = WasmProviderFactory::from_file("mock_provider.wasm")?;
    let provider = factory.create_provider(HashMap::new()).await?;

    let state = provider.create(&test_resource()).await?;
    assert!(state.identifier.is_some());

    let read = provider.read(&test_id(), state.identifier.as_deref()).await?;
    assert_eq!(state.attributes, read.attributes);
}
```

- WIT types ↔ Core types conversion tests (`wasm_convert.rs`)
- Precompile cache generation/loading tests

### Phase 2-3 (AWS/AWSCC Provider)

- Existing provider tests pass with WASM compilation
- E2E: WASM provider runs `plan` / `apply` correctly

### Phase 4 (Distribution)

- CI: `.wasm` build job
- Download/cache tests
- Precompile cache invalidation on Wasmtime version change

### CI Pipeline Addition

```yaml
wasm-build:
  - cargo build --target wasm32-wasip2 -p carina-provider-aws --release
  - cargo build --target wasm32-wasip2 -p carina-provider-awscc --release

wasm-integration:
  - cargo test -p carina-plugin-host --features wasm-tests
```
