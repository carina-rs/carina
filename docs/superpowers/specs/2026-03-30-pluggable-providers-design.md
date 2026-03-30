# Pluggable Providers via External Process + JSON-RPC

**Date**: 2026-03-30
**Status**: Draft

## Overview

Make Carina's provider system pluggable by running providers as external processes that communicate via stdin/stdout JSON-RPC. All providers — including the existing `aws` and `awscc` — become standalone binaries that are declared in `.crn` files, automatically downloaded, cached, and loaded at runtime.

## Goals

- Anyone can write a Carina provider (GCP, Azure, custom infra) without modifying Carina itself
- All providers are external — Carina ships with zero built-in providers
- Providers are written in Rust as standalone binaries
- Providers use AWS SDK and other libraries directly — no sandboxing restrictions
- Providers are declared in `.crn` files and automatically downloaded/cached

## Non-Goals

- Multi-language provider support (Rust only for now, but the process model makes future language support trivial)
- Feature flags for selective compilation (superseded by full externalization)

## Architecture

### External Process + stdin/stdout JSON-RPC

Each provider is a standalone Rust binary. Carina spawns it as a child process and communicates via JSON-RPC over stdin/stdout.

```
Carina (Host)                    Provider (Child Process)
    |                                    |
    |-- spawn process ------------------>|
    |                                    |-- ready
    |-- JSON-RPC: {"method":"create"} -->|
    |                                    |-- calls AWS SDK directly
    |<-- JSON-RPC: {"result": State} ----|
    |                                    |
    |-- JSON-RPC: {"method":"shutdown"}->|
    |                                    |-- exit
```

**Key advantages over Wasm:**
- Providers use AWS SDK, HTTP clients, TLS — everything works natively
- No host function bridging for network I/O
- No ABI compatibility issues (process boundary)
- Proven model (Terraform go-plugin, LSP protocol)

### JSON-RPC Protocol

Single-line JSON messages delimited by newlines over stdin/stdout. Stderr is forwarded for logging.

**Request format:**
```json
{"jsonrpc":"2.0","id":1,"method":"read","params":{...}}
```

**Response format:**
```json
{"jsonrpc":"2.0","id":1,"result":{...}}
```

**Error format:**
```json
{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"..."}}
```

### RPC Methods

| Method | Description |
|--------|-------------|
| `provider_info` | Return name, display_name |
| `validate_config` | Validate provider block attributes |
| `schemas` | Return all resource schemas |
| `initialize` | Initialize provider with config attributes |
| `read` | Read current resource state |
| `create` | Create a new resource |
| `update` | Update an existing resource |
| `delete` | Delete an existing resource |
| `normalize_desired` | Normalize desired resources (optional) |
| `normalize_state` | Normalize read-back state (optional) |
| `shutdown` | Graceful shutdown |

### Process Lifecycle

1. Carina spawns the provider binary with no arguments
2. Provider writes a ready message to stdout: `{"jsonrpc":"2.0","method":"ready","params":{}}`
3. Carina sends `provider_info` to confirm the provider name
4. Carina sends `validate_config` and `schemas` during planning
5. Carina sends `initialize` with provider config before any CRUD operations
6. Carina sends `read`/`create`/`update`/`delete` as needed
7. Carina sends `shutdown` when done — provider exits

## Provider Declaration in `.crn`

Providers are declared in the existing `provider` block with `source` and `version` attributes:

```crn
provider awscc {
  source = "github.com/carina-rs/carina-provider-awscc"
  version = "0.5.0"
  region = awscc.Region.ap_northeast_1
}

provider gcp {
  source = "github.com/example/carina-provider-gcp"
  version = "1.0.0"
  region = "asia-northeast1"
}

# Local development
provider custom {
  source = "file:///path/to/carina-provider-custom"
}
```

`source` and `version` are extracted from `attributes` during parsing (same pattern as `default_tags`) and are not passed to the provider.

### Parser Change

```rust
pub struct ProviderConfig {
    pub name: String,
    pub attributes: HashMap<String, Value>,
    pub default_tags: HashMap<String, Value>,
    pub source: Option<String>,    // new
    pub version: Option<String>,   // new
}
```

## Distribution and Installation

### Cache Structure

```
~/.carina/
  providers/
    cache/
      github.com/
        carina-rs/
          carina-provider-awscc/
            0.5.0/
              carina-provider-awscc          (binary)
        example/
          carina-provider-gcp/
            1.0.0/
              carina-provider-gcp            (binary)
```

### Download Flow

1. Parse `.crn` — extract `source`/`version` from provider blocks
2. Check `~/.carina/providers/cache/` for existing binary
3. If missing, download from GitHub Releases (platform-specific binary)
4. Verify SHA256 against `carina.lock`
5. Spawn binary as child process

### Lock File (`carina.lock`)

```toml
[[provider]]
name = "awscc"
source = "github.com/carina-rs/carina-provider-awscc"
version = "0.5.0"
sha256 = "abc123..."

[[provider]]
name = "gcp"
source = "github.com/example/carina-provider-gcp"
version = "1.0.0"
sha256 = "def456..."
```

- Auto-generated/updated on `carina plan` / `carina apply`
- SHA256 integrity check for provider binaries
- Committed to Git for reproducibility

## Crate Structure

### New Crates

| Crate | Purpose |
|-------|---------|
| `carina-plugin-host` | Process spawning, JSON-RPC client, provider lifecycle management |
| `carina-plugin-sdk` | Provider developer SDK. Implements JSON-RPC server, provides `CarinaProvider` trait — implement the trait and call `carina_plugin_sdk::run()` |
| `carina-provider-protocol` | Shared data types for JSON-RPC messages (request/response envelopes, serializable versions of core types) |

### Dependency Graph

```
carina-core (traits, parser)
  ↑
carina-provider-protocol (shared data types, JSON-RPC message definitions)
  ↑                    ↑
carina-plugin-host     carina-plugin-sdk
  ↑                    ↑
carina-cli             provider binaries (awscc, gcp, etc.)
```

### Host-Side Implementation

```rust
/// Manages a provider child process and implements ProviderFactory
pub struct ProcessProviderFactory {
    binary_path: PathBuf,
    info: ProviderInfo,
}

impl ProviderFactory for ProcessProviderFactory { ... }

/// Wraps a running child process, implements Provider trait
pub struct ProcessProvider {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
}

impl Provider for ProcessProvider { ... }
```

## Provider Developer Experience

### Writing a Provider

```rust
use carina_plugin_sdk::prelude::*;

struct GcpProvider {
    client: Option<GcpClient>,
}

impl CarinaProvider for GcpProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "gcp".into(),
            display_name: "Google Cloud Platform".into(),
        }
    }

    fn schemas(&self) -> Vec<ResourceSchema> { vec![...] }

    fn validate_config(&self, attrs: &Map) -> Result<(), String> { Ok(()) }

    fn initialize(&mut self, attrs: &Map) -> Result<(), String> {
        // Initialize GCP client with credentials
        self.client = Some(GcpClient::new(attrs)?);
        Ok(())
    }

    fn read(&self, id: &ResourceId, identifier: Option<&str>) -> Result<State, ProviderError> {
        // Use GCP SDK directly — no sandboxing, no bridging
        let client = self.client.as_ref().unwrap();
        let resource = client.get_resource(id)?;
        Ok(State::existing(id.clone(), resource.attributes))
    }

    fn create(&self, resource: &Resource) -> Result<State, ProviderError> { ... }
    fn update(&self, ...) -> Result<State, ProviderError> { ... }
    fn delete(&self, ...) -> Result<(), ProviderError> { ... }
}

fn main() {
    carina_plugin_sdk::run(GcpProvider { client: None });
}
```

### Building

```bash
cargo build --release
# Output: target/release/carina-provider-gcp
```

Standard `cargo build` — no special target, no Wasm compilation.

### Testing

```bash
# Unit tests — direct, no IPC needed
cargo test

# Integration test — run the binary and send JSON-RPC
cargo test --test integration
```

### Publishing

Upload platform-specific binaries to GitHub Releases:

```
carina-provider-gcp v1.0.0
  ├── carina-provider-gcp-x86_64-apple-darwin
  ├── carina-provider-gcp-aarch64-apple-darwin
  ├── carina-provider-gcp-x86_64-unknown-linux-gnu
  └── carina-provider-gcp-aarch64-unknown-linux-gnu
```

## Provider Load Lifecycle

```
1. Parse .crn
   ├── Extract source/version from provider blocks
   └── Parse resource definitions
         ↓
2. Resolve provider binaries (carina-plugin-host)
   ├── file:// source → use local binary directly
   └── github.com/... source →
       ├── Check ~/.carina/providers/cache/
       ├── Download platform-specific binary if missing
       └── Verify SHA256 against carina.lock
         ↓
3. Spawn provider process
   ├── Start binary as child process
   ├── Wait for "ready" message on stdout
   ├── Send provider_info to confirm name
   └── Send schemas to collect resource schemas
         ↓
4. Register as ProcessProviderFactory
   ├── Send validate_config
   ├── Register in ProviderRouter
   └── Send initialize with config attributes
         ↓
5. Existing flow continues unchanged
   plan → apply path is the same
         ↓
6. Shutdown
   ├── Send shutdown to each provider process
   └── Wait for graceful exit
```

## Technical Risks and Mitigations

### Risk 1: Process Startup Overhead

Spawning a process per provider adds latency at startup.

**Mitigation**: Providers are long-lived — spawned once per `carina plan`/`carina apply` invocation. The startup cost is amortized over all operations. Process spawning is fast (< 10ms).

### Risk 2: Serialization Overhead

JSON-RPC over stdin/stdout adds serialization/deserialization cost per call.

**Mitigation**: IaC bottleneck is network I/O (API calls), not local serialization. JSON serialization of resource state is negligible compared to API round-trips.

### Risk 3: Provider Crash Isolation

If a provider process crashes, Carina needs to handle it gracefully.

**Mitigation**: Monitor child process status. If the process exits unexpectedly, report a clear error with stderr output. No risk of corrupting Carina's own state.

### Risk 4: Cross-Platform Binary Distribution

Each provider needs platform-specific binaries (unlike Wasm's universal `.wasm`).

**Mitigation**: GitHub Actions can build for all major platforms. Standard Rust cross-compilation tooling works well. `carina` detects the current platform and downloads the correct binary.

## Migration Strategy

1. **Phase 1**: Create `carina-plugin-host`, `carina-plugin-sdk`, `carina-provider-protocol`. Convert Mock provider to external process for E2E validation.
2. **Phase 2**: Convert `awscc` provider to external process. AWS SDK works directly — no bridging needed.
3. **Phase 3**: Convert `aws` provider to external process (or deprecate in favor of `awscc`).
4. **Phase 4**: Remove direct provider dependencies from `carina-cli`. Implement download/install mechanism.

Phase 1 validates the full pipeline with minimal risk.
