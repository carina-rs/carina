# Pluggable Providers Phase 1: External Process Plugin Infrastructure + Mock Provider

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the external process plugin infrastructure and validate it by converting the Mock provider to a standalone binary that communicates with Carina via stdin/stdout JSON-RPC.

**Architecture:** Three new crates — `carina-provider-protocol` (shared serializable types + JSON-RPC message format), `carina-plugin-sdk` (provider developer SDK with JSON-RPC server), `carina-plugin-host` (process spawning + JSON-RPC client). The Mock provider is rebuilt as an external binary to prove the E2E pipeline works. `carina-cli` wiring is updated to spawn and communicate with provider processes.

**Tech Stack:** `serde_json` for JSON-RPC serialization, `std::process` for child process management, `std::io::BufReader`/`BufWriter` for stdin/stdout communication.

**Spec:** `docs/superpowers/specs/2026-03-30-pluggable-providers-design.md`

---

## File Structure

### New Crates

```
carina-provider-protocol/
  Cargo.toml
  src/
    lib.rs              — Re-exports all protocol types
    types.rs            — Serializable versions of ResourceId, State, Value, etc.
    jsonrpc.rs          — JSON-RPC request/response/error envelope types
    methods.rs          — Per-method request params and response result types

carina-plugin-sdk/
  Cargo.toml
  src/
    lib.rs              — CarinaProvider trait + run() entry point (JSON-RPC server loop)

carina-plugin-host/
  Cargo.toml
  src/
    lib.rs              — Re-exports
    process.rs          — Spawn child process, JSON-RPC client over stdin/stdout
    factory.rs          — ProcessProviderFactory: implements ProviderFactory
    provider.rs         — ProcessProvider: implements Provider trait
    convert.rs          — Type conversions between carina-core and protocol types

carina-provider-mock-process/
  Cargo.toml
  src/
    main.rs             — MockProvider as standalone binary using carina-plugin-sdk
```

### Modified Files

```
Cargo.toml                          — Add new workspace members
carina-core/src/parser/mod.rs       — Add source/version to ProviderConfig
carina-cli/Cargo.toml               — Add carina-plugin-host dependency
carina-cli/src/wiring.rs            — Load providers via process spawning
```

---

## Task 1: Create `carina-provider-protocol` crate with serializable types and JSON-RPC format

The protocol crate defines JSON-serializable types for the process boundary and the JSON-RPC message envelope format.

**Files:**
- Create: `carina-provider-protocol/Cargo.toml`
- Create: `carina-provider-protocol/src/lib.rs`
- Create: `carina-provider-protocol/src/types.rs`
- Create: `carina-provider-protocol/src/jsonrpc.rs`
- Create: `carina-provider-protocol/src/methods.rs`
- Modify: `Cargo.toml` (workspace root)

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "carina-provider-protocol"
version = "0.1.0"
edition = "2024"
license = "MIT"

[lib]
doctest = false

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: Create `src/types.rs` with serializable protocol types**

```rust
//! Serializable protocol types for host-guest communication.
//!
//! These mirror carina-core types but are JSON-serializable across the process boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Mirrors `carina_core::resource::ResourceId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ResourceId {
    pub provider: String,
    pub resource_type: String,
    pub name: String,
}

/// Mirrors `carina_core::resource::Value`.
///
/// Only includes variants that can cross the process boundary.
/// `ResourceRef`, `Interpolation`, `FunctionCall`, `Closure` are resolved
/// before reaching the provider, so they are excluded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
    List(Vec<Value>),
    Map(HashMap<String, Value>),
}

/// Mirrors `carina_core::resource::State`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub id: ResourceId,
    pub identifier: Option<String>,
    pub attributes: HashMap<String, Value>,
    pub exists: bool,
}

/// Mirrors `carina_core::resource::LifecycleConfig`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LifecycleConfig {
    pub force_delete: bool,
    pub create_before_destroy: bool,
}

/// Simplified resource for the process boundary.
/// Attributes are pre-resolved `Value`s, not `Expr`s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: ResourceId,
    pub attributes: HashMap<String, Value>,
    pub lifecycle: LifecycleConfig,
}

/// Provider metadata returned by `provider_info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
}

/// Provider error returned from operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderError {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<ResourceId>,
    #[serde(default)]
    pub is_timeout: bool,
}

/// Schema types for resource validation and completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceSchema {
    pub resource_type: String,
    pub attributes: HashMap<String, AttributeSchema>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub data_source: bool,
    #[serde(default)]
    pub name_attribute: Option<String>,
    #[serde(default)]
    pub force_replace: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttributeSchema {
    pub name: String,
    pub attr_type: AttributeType,
    pub required: bool,
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub create_only: bool,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub write_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AttributeType {
    String,
    Int,
    Float,
    Bool,
    #[serde(rename = "string_enum")]
    StringEnum { values: Vec<String> },
    #[serde(rename = "list")]
    List { inner: Box<AttributeType> },
    #[serde(rename = "map")]
    Map(Box<AttributeType>),
    #[serde(rename = "struct")]
    Struct { name: String, fields: Vec<StructField> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StructField {
    pub name: String,
    pub field_type: AttributeType,
    pub required: bool,
    #[serde(default)]
    pub description: Option<String>,
}
```

- [ ] **Step 3: Create `src/jsonrpc.rs` with JSON-RPC envelope types**

```rust
//! JSON-RPC 2.0 message types for stdin/stdout communication.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// JSON-RPC request sent from host to provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<JsonValue>,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: Some(serde_json::to_value(params).unwrap_or(JsonValue::Null)),
        }
    }
}

/// JSON-RPC response sent from provider to host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    pub fn success(id: u64, result: impl Serialize) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(JsonValue::Null)),
            error: None,
        }
    }

    pub fn error(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

/// JSON-RPC error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<JsonValue>,
}

/// Notification sent from provider to host (no id, no response expected).
/// Used for the "ready" message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<JsonValue>,
}

impl Notification {
    pub fn ready() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: "ready".into(),
            params: None,
        }
    }
}
```

- [ ] **Step 4: Create `src/methods.rs` with per-method request/response types**

```rust
//! Per-method request params and response result types.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::types::*;

// -- provider_info --

#[derive(Debug, Serialize, Deserialize)]
pub struct ProviderInfoResult {
    pub info: ProviderInfo,
}

// -- validate_config --

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateConfigParams {
    pub attributes: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ValidateConfigResult {
    pub error: Option<String>,
}

// -- schemas --

#[derive(Debug, Serialize, Deserialize)]
pub struct SchemasResult {
    pub schemas: Vec<ResourceSchema>,
}

// -- initialize --

#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeParams {
    pub attributes: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InitializeResult {
    pub ok: bool,
}

// -- read --

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadParams {
    pub id: ResourceId,
    pub identifier: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReadResult {
    pub state: State,
}

// -- create --

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateParams {
    pub resource: Resource,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CreateResult {
    pub state: State,
}

// -- update --

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateParams {
    pub id: ResourceId,
    pub identifier: String,
    pub from: State,
    pub to: Resource,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateResult {
    pub state: State,
}

// -- delete --

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteParams {
    pub id: ResourceId,
    pub identifier: String,
    pub lifecycle: LifecycleConfig,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DeleteResult {
    pub ok: bool,
}

// -- normalize_desired --

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeDesiredParams {
    pub resources: Vec<Resource>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeDesiredResult {
    pub resources: Vec<Resource>,
}

// -- normalize_state --

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeStateParams {
    pub states: HashMap<String, State>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NormalizeStateResult {
    pub states: HashMap<String, State>,
}
```

- [ ] **Step 5: Create `src/lib.rs` that re-exports**

```rust
pub mod jsonrpc;
pub mod methods;
pub mod types;

pub use jsonrpc::*;
pub use methods::*;
pub use types::*;
```

- [ ] **Step 6: Add to workspace and verify build**

Add `"carina-provider-protocol"` to the `members` list in the root `Cargo.toml`.

Run: `cargo build -p carina-provider-protocol`
Expected: BUILD SUCCESS

- [ ] **Step 7: Write tests for serialization round-trips**

Add to `carina-provider-protocol/src/types.rs` at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_roundtrip() {
        let values = vec![
            Value::String("hello".into()),
            Value::Int(42),
            Value::Float(3.14),
            Value::Bool(true),
            Value::List(vec![Value::Int(1), Value::Int(2)]),
            Value::Map(HashMap::from([
                ("key".into(), Value::String("val".into())),
            ])),
        ];

        for value in values {
            let json = serde_json::to_string(&value).unwrap();
            let back: Value = serde_json::from_str(&json).unwrap();
            assert_eq!(value, back);
        }
    }

    #[test]
    fn test_state_roundtrip() {
        let state = State {
            id: ResourceId {
                provider: "mock".into(),
                resource_type: "test.resource".into(),
                name: "my-resource".into(),
            },
            identifier: Some("mock-id".into()),
            attributes: HashMap::from([
                ("name".into(), Value::String("test".into())),
            ]),
            exists: true,
        };

        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(state.id, back.id);
        assert_eq!(state.identifier, back.identifier);
        assert_eq!(state.exists, back.exists);
    }

    #[test]
    fn test_attribute_type_roundtrip() {
        let attr = AttributeType::Struct {
            name: "Config".into(),
            fields: vec![StructField {
                name: "enabled".into(),
                field_type: AttributeType::Bool,
                required: true,
                description: None,
            }],
        };

        let json = serde_json::to_string(&attr).unwrap();
        let back: AttributeType = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }
}
```

Add to `carina-provider-protocol/src/jsonrpc.rs` at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_serialization() {
        let req = Request::new(1, "read", serde_json::json!({"id": "test"}));
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
        assert!(json.contains("\"method\":\"read\""));
    }

    #[test]
    fn test_response_success() {
        let resp = Response::success(1, serde_json::json!({"ok": true}));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"result\""));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn test_response_error() {
        let resp = Response::error(1, -1, "something failed");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"error\""));
        assert!(!json.contains("\"result\""));
    }

    #[test]
    fn test_notification_ready() {
        let notif = Notification::ready();
        let json = serde_json::to_string(&notif).unwrap();
        assert!(json.contains("\"method\":\"ready\""));
        assert!(!json.contains("\"id\""));
    }
}
```

Run: `cargo test -p carina-provider-protocol`
Expected: 7 tests PASS

- [ ] **Step 8: Commit**

```bash
git add carina-provider-protocol/ Cargo.toml
git commit -m "feat: add carina-provider-protocol crate with serializable types and JSON-RPC format"
```

---

## Task 2: Create `carina-plugin-sdk` crate (provider developer SDK)

The SDK provides a `CarinaProvider` trait and a `run()` function that starts a JSON-RPC server loop reading from stdin and writing to stdout.

**Files:**
- Create: `carina-plugin-sdk/Cargo.toml`
- Create: `carina-plugin-sdk/src/lib.rs`

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "carina-plugin-sdk"
version = "0.1.0"
edition = "2024"
license = "MIT"

[lib]
doctest = false

[dependencies]
carina-provider-protocol = { path = "../carina-provider-protocol" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 2: Create `src/lib.rs` with `CarinaProvider` trait and `run()` function**

```rust
//! Carina provider SDK for building external process providers.
//!
//! Implement the `CarinaProvider` trait and call `carina_plugin_sdk::run(provider)`
//! in your `main()` to start the JSON-RPC server loop.

pub use carina_provider_protocol as protocol;
pub use carina_provider_protocol::types;

use carina_provider_protocol::jsonrpc::{Notification, Request, Response};
use carina_provider_protocol::methods;
use carina_provider_protocol::types::*;
use std::collections::HashMap;
use std::io::{self, BufRead, Write};

/// Trait that provider authors implement.
pub trait CarinaProvider {
    /// Return provider name and display name.
    fn info(&self) -> ProviderInfo;

    /// Return all resource schemas this provider supports.
    fn schemas(&self) -> Vec<ResourceSchema>;

    /// Validate provider configuration attributes.
    fn validate_config(&self, attrs: &HashMap<String, Value>) -> Result<(), String>;

    /// Initialize the provider with configuration.
    /// Called once before any CRUD operations.
    fn initialize(&mut self, attrs: &HashMap<String, Value>) -> Result<(), String> {
        let _ = attrs;
        Ok(())
    }

    /// Read current state of a resource.
    fn read(&self, id: &ResourceId, identifier: Option<&str>) -> Result<State, ProviderError>;

    /// Create a new resource.
    fn create(&self, resource: &Resource) -> Result<State, ProviderError>;

    /// Update an existing resource.
    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> Result<State, ProviderError>;

    /// Delete an existing resource.
    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> Result<(), ProviderError>;

    /// Normalize desired resources (optional).
    fn normalize_desired(&self, resources: Vec<Resource>) -> Vec<Resource> {
        resources
    }

    /// Normalize read-back state (optional).
    fn normalize_state(&self, states: HashMap<String, State>) -> HashMap<String, State> {
        states
    }
}

/// Start the JSON-RPC server loop.
///
/// Reads JSON-RPC requests from stdin (one per line), dispatches to the
/// provider, and writes JSON-RPC responses to stdout (one per line).
///
/// Call this from `main()`:
/// ```ignore
/// fn main() {
///     carina_plugin_sdk::run(MyProvider::default());
/// }
/// ```
pub fn run(mut provider: impl CarinaProvider) {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = stdin.lock();
    let mut writer = stdout.lock();

    // Send ready notification
    let ready = Notification::ready();
    let ready_json = serde_json::to_string(&ready).expect("Failed to serialize ready");
    writeln!(writer, "{ready_json}").expect("Failed to write ready");
    writer.flush().expect("Failed to flush");

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => break, // EOF — host closed stdin
            Ok(_) => {}
            Err(e) => {
                eprintln!("Failed to read stdin: {e}");
                break;
            }
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let request: Request = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Failed to parse JSON-RPC request: {e}");
                continue;
            }
        };

        let response = dispatch(&mut provider, &request);

        let resp_json = serde_json::to_string(&response).expect("Failed to serialize response");
        writeln!(writer, "{resp_json}").expect("Failed to write response");
        writer.flush().expect("Failed to flush");

        // Exit after shutdown
        if request.method == "shutdown" {
            break;
        }
    }
}

fn dispatch(provider: &mut impl CarinaProvider, request: &Request) -> Response {
    let id = request.id;

    match request.method.as_str() {
        "provider_info" => {
            let info = provider.info();
            Response::success(id, methods::ProviderInfoResult { info })
        }

        "validate_config" => {
            let params: methods::ValidateConfigParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let error = provider.validate_config(&params.attributes).err();
            Response::success(id, methods::ValidateConfigResult { error })
        }

        "schemas" => {
            let schemas = provider.schemas();
            Response::success(id, methods::SchemasResult { schemas })
        }

        "initialize" => {
            let params: methods::InitializeParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.initialize(&params.attributes) {
                Ok(()) => Response::success(id, methods::InitializeResult { ok: true }),
                Err(e) => Response::error(id, -1, e),
            }
        }

        "read" => {
            let params: methods::ReadParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.read(&params.id, params.identifier.as_deref()) {
                Ok(state) => Response::success(id, methods::ReadResult { state }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "create" => {
            let params: methods::CreateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.create(&params.resource) {
                Ok(state) => Response::success(id, methods::CreateResult { state }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "update" => {
            let params: methods::UpdateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.update(&params.id, &params.identifier, &params.from, &params.to) {
                Ok(state) => Response::success(id, methods::UpdateResult { state }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "delete" => {
            let params: methods::DeleteParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            match provider.delete(&params.id, &params.identifier, &params.lifecycle) {
                Ok(()) => Response::success(id, methods::DeleteResult { ok: true }),
                Err(e) => Response::error(id, -1, e.message),
            }
        }

        "normalize_desired" => {
            let params: methods::NormalizeDesiredParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let resources = provider.normalize_desired(params.resources);
            Response::success(id, methods::NormalizeDesiredResult { resources })
        }

        "normalize_state" => {
            let params: methods::NormalizeStateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let states = provider.normalize_state(params.states);
            Response::success(id, methods::NormalizeStateResult { states })
        }

        "shutdown" => Response::success(id, serde_json::json!({"ok": true})),

        _ => Response::error(id, -32601, format!("Unknown method: {}", request.method)),
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(
    params: &Option<serde_json::Value>,
) -> Result<T, String> {
    match params {
        Some(v) => serde_json::from_value(v.clone()).map_err(|e| format!("Invalid params: {e}")),
        None => serde_json::from_value(serde_json::json!({}))
            .map_err(|e| format!("Missing params: {e}")),
    }
}
```

- [ ] **Step 3: Add to workspace and verify build**

Add `"carina-plugin-sdk"` to the `members` list in the root `Cargo.toml`.

Run: `cargo build -p carina-plugin-sdk`
Expected: BUILD SUCCESS

- [ ] **Step 4: Commit**

```bash
git add carina-plugin-sdk/ Cargo.toml
git commit -m "feat: add carina-plugin-sdk crate with CarinaProvider trait and JSON-RPC server"
```

---

## Task 3: Create `carina-provider-mock-process` (Mock provider as external binary)

Reimplement the existing Mock provider as a standalone binary using the SDK.

**Files:**
- Create: `carina-provider-mock-process/Cargo.toml`
- Create: `carina-provider-mock-process/src/main.rs`

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "carina-provider-mock-process"
version = "0.1.0"
edition = "2024"
license = "MIT"

[[bin]]
name = "carina-provider-mock-process"
path = "src/main.rs"

[dependencies]
carina-plugin-sdk = { path = "../carina-plugin-sdk" }
carina-provider-protocol = { path = "../carina-provider-protocol" }
serde_json = "1"
```

- [ ] **Step 2: Create `src/main.rs`**

```rust
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::CarinaProvider;
use std::collections::HashMap;
use std::sync::Mutex;

struct MockProcessProvider {
    states: Mutex<HashMap<String, HashMap<String, Value>>>,
}

impl Default for MockProcessProvider {
    fn default() -> Self {
        Self {
            states: Mutex::new(HashMap::new()),
        }
    }
}

impl MockProcessProvider {
    fn resource_key(id: &ResourceId) -> String {
        format!("{}.{}", id.resource_type, id.name)
    }
}

impl CarinaProvider for MockProcessProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "mock".into(),
            display_name: "Mock Provider (Process)".into(),
        }
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![]
    }

    fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn read(&self, id: &ResourceId, _identifier: Option<&str>) -> Result<State, ProviderError> {
        let states = self.states.lock().unwrap();
        let key = Self::resource_key(id);

        if let Some(attrs) = states.get(&key) {
            Ok(State {
                id: id.clone(),
                identifier: Some("mock-id".into()),
                attributes: attrs.clone(),
                exists: true,
            })
        } else {
            Ok(State {
                id: id.clone(),
                identifier: None,
                attributes: HashMap::new(),
                exists: false,
            })
        }
    }

    fn create(&self, resource: &Resource) -> Result<State, ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(&resource.id);
        states.insert(key, resource.attributes.clone());

        Ok(State {
            id: resource.id.clone(),
            identifier: Some("mock-id".into()),
            attributes: resource.attributes.clone(),
            exists: true,
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _from: &State,
        to: &Resource,
    ) -> Result<State, ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        states.insert(key, to.attributes.clone());

        Ok(State {
            id: id.clone(),
            identifier: Some("mock-id".into()),
            attributes: to.attributes.clone(),
            exists: true,
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        _identifier: &str,
        _lifecycle: &LifecycleConfig,
    ) -> Result<(), ProviderError> {
        let mut states = self.states.lock().unwrap();
        let key = Self::resource_key(id);
        states.remove(&key);
        Ok(())
    }
}

fn main() {
    carina_plugin_sdk::run(MockProcessProvider::default());
}
```

- [ ] **Step 3: Add to workspace and build**

Add `"carina-provider-mock-process"` to the `members` list in the root `Cargo.toml`.

Run: `cargo build -p carina-provider-mock-process`
Expected: BUILD SUCCESS

- [ ] **Step 4: Manual smoke test — run the binary and send JSON-RPC**

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"provider_info","params":{}}
{"jsonrpc":"2.0","id":2,"method":"shutdown","params":{}}' | cargo run -p carina-provider-mock-process
```

Expected output (3 lines):
```
{"jsonrpc":"2.0","method":"ready"}
{"jsonrpc":"2.0","id":1,"result":{"info":{"name":"mock","display_name":"Mock Provider (Process)"}}}
{"jsonrpc":"2.0","id":2,"result":{"ok":true}}
```

- [ ] **Step 5: Commit**

```bash
git add carina-provider-mock-process/ Cargo.toml
git commit -m "feat: add carina-provider-mock-process — mock provider as external binary"
```

---

## Task 4: Create `carina-plugin-host` crate (process spawning + JSON-RPC client)

The host crate spawns provider binaries as child processes, communicates via JSON-RPC, and wraps them as `ProviderFactory`/`Provider` implementations.

**Files:**
- Create: `carina-plugin-host/Cargo.toml`
- Create: `carina-plugin-host/src/lib.rs`
- Create: `carina-plugin-host/src/process.rs`
- Create: `carina-plugin-host/src/factory.rs`
- Create: `carina-plugin-host/src/provider.rs`
- Create: `carina-plugin-host/src/convert.rs`

- [ ] **Step 1: Create `Cargo.toml`**

```toml
[package]
name = "carina-plugin-host"
version = "0.1.0"
edition = "2024"
license = "MIT"

[lib]
doctest = false

[dependencies]
carina-core = { path = "../carina-core" }
carina-provider-protocol = { path = "../carina-provider-protocol" }
serde_json = "1"
log = "0.4"
futures = "0.3"
```

- [ ] **Step 2: Create `src/convert.rs` — type conversions between core and protocol**

```rust
//! Conversions between carina-core types and carina-provider-protocol types.

use std::collections::HashMap;

use carina_core::resource::{
    LifecycleConfig as CoreLifecycle, Resource as CoreResource, ResourceId as CoreResourceId,
    State as CoreState, Value as CoreValue,
};
use carina_provider_protocol::types::{
    LifecycleConfig as ProtoLifecycle, Resource as ProtoResource, ResourceId as ProtoResourceId,
    State as ProtoState, Value as ProtoValue,
};

// -- ResourceId --

pub fn core_to_proto_resource_id(id: &CoreResourceId) -> ProtoResourceId {
    ProtoResourceId {
        provider: id.provider.clone(),
        resource_type: id.resource_type.clone(),
        name: id.name.clone(),
    }
}

pub fn proto_to_core_resource_id(id: &ProtoResourceId) -> CoreResourceId {
    CoreResourceId::with_provider(&id.provider, &id.resource_type, &id.name)
}

// -- Value --

pub fn core_to_proto_value(v: &CoreValue) -> ProtoValue {
    match v {
        CoreValue::String(s) => ProtoValue::String(s.clone()),
        CoreValue::Int(i) => ProtoValue::Int(*i),
        CoreValue::Float(f) => ProtoValue::Float(*f),
        CoreValue::Bool(b) => ProtoValue::Bool(*b),
        CoreValue::List(l) => ProtoValue::List(l.iter().map(core_to_proto_value).collect()),
        CoreValue::Map(m) => ProtoValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), core_to_proto_value(v)))
                .collect(),
        ),
        // ResourceRef, Interpolation, FunctionCall, Closure, Secret
        // should be resolved before reaching the provider.
        _ => ProtoValue::String(format!("{v:?}")),
    }
}

pub fn proto_to_core_value(v: &ProtoValue) -> CoreValue {
    match v {
        ProtoValue::String(s) => CoreValue::String(s.clone()),
        ProtoValue::Int(i) => CoreValue::Int(*i),
        ProtoValue::Float(f) => CoreValue::Float(*f),
        ProtoValue::Bool(b) => CoreValue::Bool(*b),
        ProtoValue::List(l) => CoreValue::List(l.iter().map(proto_to_core_value).collect()),
        ProtoValue::Map(m) => CoreValue::Map(
            m.iter()
                .map(|(k, v)| (k.clone(), proto_to_core_value(v)))
                .collect(),
        ),
    }
}

pub fn core_to_proto_value_map(m: &HashMap<String, CoreValue>) -> HashMap<String, ProtoValue> {
    m.iter()
        .map(|(k, v)| (k.clone(), core_to_proto_value(v)))
        .collect()
}

pub fn proto_to_core_value_map(m: &HashMap<String, ProtoValue>) -> HashMap<String, CoreValue> {
    m.iter()
        .map(|(k, v)| (k.clone(), proto_to_core_value(v)))
        .collect()
}

// -- State --

pub fn core_to_proto_state(s: &CoreState) -> ProtoState {
    ProtoState {
        id: core_to_proto_resource_id(&s.id),
        identifier: s.identifier.clone(),
        attributes: core_to_proto_value_map(&s.attributes),
        exists: s.exists,
    }
}

pub fn proto_to_core_state(s: &ProtoState) -> CoreState {
    let id = proto_to_core_resource_id(&s.id);
    if s.exists {
        let mut state = CoreState::existing(id, proto_to_core_value_map(&s.attributes));
        if let Some(ref ident) = s.identifier {
            state = state.with_identifier(ident);
        }
        state
    } else {
        CoreState::not_found(id)
    }
}

// -- Resource --

pub fn core_to_proto_resource(r: &CoreResource) -> ProtoResource {
    ProtoResource {
        id: core_to_proto_resource_id(&r.id),
        attributes: core_to_proto_value_map(&r.resolved_attributes()),
        lifecycle: core_to_proto_lifecycle(&r.lifecycle),
    }
}

// -- LifecycleConfig --

pub fn core_to_proto_lifecycle(l: &CoreLifecycle) -> ProtoLifecycle {
    ProtoLifecycle {
        force_delete: l.force_delete,
        create_before_destroy: l.create_before_destroy,
    }
}
```

- [ ] **Step 3: Create `src/process.rs` — spawn child process + JSON-RPC client**

```rust
//! Spawn provider binary as child process and communicate via JSON-RPC over stdin/stdout.

use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use carina_provider_protocol::jsonrpc::{Request, Response};

/// A running provider child process with JSON-RPC communication.
pub struct ProviderProcess {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
    writer: BufWriter<std::process::ChildStdin>,
    next_id: AtomicU64,
}

impl ProviderProcess {
    /// Spawn a provider binary and wait for the "ready" notification.
    pub fn spawn(binary_path: &Path) -> Result<Self, String> {
        let mut child = Command::new(binary_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit()) // Forward stderr for logging
            .spawn()
            .map_err(|e| format!("Failed to spawn provider {}: {e}", binary_path.display()))?;

        let stdout = child
            .stdout
            .take()
            .ok_or("Failed to capture provider stdout")?;
        let stdin = child
            .stdin
            .take()
            .ok_or("Failed to capture provider stdin")?;

        let mut reader = BufReader::new(stdout);
        let writer = BufWriter::new(stdin);

        // Wait for ready notification
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("Failed to read ready message: {e}"))?;

        let trimmed = line.trim();
        if !trimmed.contains("\"ready\"") {
            return Err(format!(
                "Expected ready notification, got: {trimmed}"
            ));
        }

        Ok(Self {
            child,
            reader,
            writer,
            next_id: AtomicU64::new(1),
        })
    }

    /// Send a JSON-RPC request and wait for the response.
    pub fn call<P: serde::Serialize, R: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);

        let req_json =
            serde_json::to_string(&request).map_err(|e| format!("Serialize failed: {e}"))?;

        writeln!(self.writer, "{req_json}").map_err(|e| format!("Write failed: {e}"))?;
        self.writer
            .flush()
            .map_err(|e| format!("Flush failed: {e}"))?;

        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .map_err(|e| format!("Read failed: {e}"))?;

        let response: Response =
            serde_json::from_str(line.trim()).map_err(|e| format!("Parse response failed: {e}"))?;

        if let Some(err) = response.error {
            return Err(format!("RPC error ({}): {}", err.code, err.message));
        }

        let result = response
            .result
            .ok_or_else(|| "Response has neither result nor error".to_string())?;

        serde_json::from_value(result).map_err(|e| format!("Deserialize result failed: {e}"))
    }

    /// Send shutdown and wait for process to exit.
    pub fn shutdown(&mut self) {
        let _ = self.call::<serde_json::Value, serde_json::Value>(
            "shutdown",
            &serde_json::json!({}),
        );
        let _ = self.child.wait();
    }
}

impl Drop for ProviderProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}
```

- [ ] **Step 4: Create `src/provider.rs` — ProcessProvider implementing Provider trait**

```rust
//! ProcessProvider wraps a ProviderProcess and implements the carina-core Provider trait.

use std::sync::Mutex;

use carina_core::provider::{BoxFuture, Provider, ProviderError, ProviderResult};
use carina_core::resource::{LifecycleConfig, Resource, ResourceId, State};
use carina_provider_protocol::methods;

use crate::convert;
use crate::process::ProviderProcess;

pub struct ProcessProvider {
    process: Mutex<ProviderProcess>,
    name: &'static str,
}

impl ProcessProvider {
    pub fn new(process: ProviderProcess, name: String) -> Self {
        let name_static: &'static str = Box::leak(name.into_boxed_str());
        Self {
            process: Mutex::new(process),
            name: name_static,
        }
    }
}

impl Provider for ProcessProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let params = methods::ReadParams {
            id: convert::core_to_proto_resource_id(id),
            identifier: identifier.map(String::from),
        };
        Box::pin(async move {
            let mut process = self.process.lock().map_err(|e| {
                ProviderError::new(format!("Process lock poisoned: {e}"))
            })?;
            let result: methods::ReadResult = process
                .call("read", &params)
                .map_err(|e| ProviderError::new(e))?;
            Ok(convert::proto_to_core_state(&result.state))
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let params = methods::CreateParams {
            resource: convert::core_to_proto_resource(resource),
        };
        Box::pin(async move {
            let mut process = self.process.lock().map_err(|e| {
                ProviderError::new(format!("Process lock poisoned: {e}"))
            })?;
            let result: methods::CreateResult = process
                .call("create", &params)
                .map_err(|e| ProviderError::new(e))?;
            Ok(convert::proto_to_core_state(&result.state))
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let params = methods::UpdateParams {
            id: convert::core_to_proto_resource_id(id),
            identifier: identifier.to_string(),
            from: convert::core_to_proto_state(from),
            to: convert::core_to_proto_resource(to),
        };
        Box::pin(async move {
            let mut process = self.process.lock().map_err(|e| {
                ProviderError::new(format!("Process lock poisoned: {e}"))
            })?;
            let result: methods::UpdateResult = process
                .call("update", &params)
                .map_err(|e| ProviderError::new(e))?;
            Ok(convert::proto_to_core_state(&result.state))
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let params = methods::DeleteParams {
            id: convert::core_to_proto_resource_id(id),
            identifier: identifier.to_string(),
            lifecycle: convert::core_to_proto_lifecycle(lifecycle),
        };
        Box::pin(async move {
            let mut process = self.process.lock().map_err(|e| {
                ProviderError::new(format!("Process lock poisoned: {e}"))
            })?;
            let _result: methods::DeleteResult = process
                .call("delete", &params)
                .map_err(|e| ProviderError::new(e))?;
            Ok(())
        })
    }
}
```

- [ ] **Step 5: Create `src/factory.rs` — ProcessProviderFactory implementing ProviderFactory**

```rust
//! ProcessProviderFactory spawns a provider process and implements ProviderFactory.

use std::collections::HashMap;
use std::path::PathBuf;

use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderNormalizer};
use carina_core::resource::Value;
use carina_core::schema::ResourceSchema;
use carina_provider_protocol::methods;
use carina_provider_protocol::types::ProviderInfo;

use crate::convert;
use crate::process::ProviderProcess;
use crate::provider::ProcessProvider;

pub struct ProcessProviderFactory {
    binary_path: PathBuf,
    info: ProviderInfo,
    name_static: &'static str,
    display_name_static: &'static str,
}

impl ProcessProviderFactory {
    /// Create a new ProcessProviderFactory by spawning the binary and querying provider_info.
    pub fn new(binary_path: PathBuf) -> Result<Self, String> {
        let mut process = ProviderProcess::spawn(&binary_path)?;

        let result: methods::ProviderInfoResult = process
            .call("provider_info", &serde_json::json!({}))
            .map_err(|e| format!("Failed to get provider_info: {e}"))?;

        let name_static: &'static str = Box::leak(result.info.name.clone().into_boxed_str());
        let display_name_static: &'static str =
            Box::leak(result.info.display_name.clone().into_boxed_str());

        // Shut down this temporary process — a new one will be spawned for actual use
        process.shutdown();

        Ok(Self {
            binary_path,
            info: result.info,
            name_static,
            display_name_static,
        })
    }
}

impl ProviderFactory for ProcessProviderFactory {
    fn name(&self) -> &str {
        self.name_static
    }

    fn display_name(&self) -> &str {
        self.display_name_static
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let mut process = ProviderProcess::spawn(&self.binary_path)?;

        let params = methods::ValidateConfigParams {
            attributes: convert::core_to_proto_value_map(attributes),
        };
        let result: methods::ValidateConfigResult = process.call("validate_config", &params)?;

        process.shutdown();

        match result.error {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            carina_core::utils::convert_region_value(region)
        } else {
            "ap-northeast-1".to_string()
        }
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        let binary_path = self.binary_path.clone();
        let attrs = convert::core_to_proto_value_map(attributes);
        let name = self.info.name.clone();

        Box::pin(async move {
            let mut process = ProviderProcess::spawn(&binary_path)
                .expect("Failed to spawn provider process");

            let params = methods::InitializeParams { attributes: attrs };
            let _result: methods::InitializeResult = process
                .call("initialize", &params)
                .expect("Failed to initialize provider");

            Box::new(ProcessProvider::new(process, name)) as Box<dyn Provider>
        })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        // For Phase 1, return empty — Mock provider has no schemas.
        // Full schema conversion (proto → core) will be implemented in Phase 2.
        vec![]
    }
}
```

- [ ] **Step 6: Create `src/lib.rs`**

```rust
pub mod convert;
pub mod factory;
pub mod process;
pub mod provider;

pub use factory::ProcessProviderFactory;
pub use provider::ProcessProvider;
```

- [ ] **Step 7: Add to workspace and verify build**

Add `"carina-plugin-host"` to the `members` list in the root `Cargo.toml`.

Run: `cargo build -p carina-plugin-host`
Expected: BUILD SUCCESS

- [ ] **Step 8: Commit**

```bash
git add carina-plugin-host/ Cargo.toml
git commit -m "feat: add carina-plugin-host crate — process spawning, JSON-RPC client, ProcessProvider"
```

---

## Task 5: Add `source`/`version` to `ProviderConfig` in the parser

Extend the parser to extract `source` and `version` from provider blocks, following the same pattern as `default_tags`.

**Files:**
- Modify: `carina-core/src/parser/mod.rs`

- [ ] **Step 1: Write failing test for `source`/`version` parsing**

Add tests in `carina-core/src/parser/mod.rs` in the test module:

```rust
#[test]
fn test_provider_block_with_source_and_version() {
    let input = r#"
        provider mock {
            source = "github.com/carina-rs/carina-provider-mock"
            version = "0.1.0"
        }
    "#;
    let parsed = parse(input, None).unwrap();
    assert_eq!(parsed.providers.len(), 1);

    let provider = &parsed.providers[0];
    assert_eq!(provider.name, "mock");
    assert_eq!(
        provider.source.as_deref(),
        Some("github.com/carina-rs/carina-provider-mock")
    );
    assert_eq!(provider.version.as_deref(), Some("0.1.0"));
    // source and version should NOT be in attributes
    assert!(!provider.attributes.contains_key("source"));
    assert!(!provider.attributes.contains_key("version"));
}

#[test]
fn test_provider_block_without_source() {
    let input = r#"
        provider awscc {
            region = awscc.Region.ap_northeast_1
        }
    "#;
    let parsed = parse(input, None).unwrap();
    let provider = &parsed.providers[0];
    assert!(provider.source.is_none());
    assert!(provider.version.is_none());
}
```

Run: `cargo test -p carina-core test_provider_block_with_source`
Expected: FAIL — `source` field does not exist on `ProviderConfig`

- [ ] **Step 2: Add `source`/`version` fields to `ProviderConfig`**

In `carina-core/src/parser/mod.rs`, modify the `ProviderConfig` struct (around line 283):

```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    pub attributes: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub default_tags: HashMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}
```

- [ ] **Step 3: Extract `source`/`version` in `parse_provider_block`**

In the `parse_provider_block` function (around line 1717, after `default_tags` extraction), add:

```rust
    // Extract source from attributes if present
    let source = if let Some(Value::String(s)) = attributes.remove("source") {
        Some(s)
    } else {
        None
    };

    // Extract version from attributes if present
    let version = if let Some(Value::String(v)) = attributes.remove("version") {
        Some(v)
    } else {
        None
    };

    Ok(ProviderConfig {
        name,
        attributes,
        default_tags,
        source,
        version,
    })
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p carina-core test_provider_block_with_source`
Expected: 2 tests PASS

Run: `cargo test -p carina-core`
Expected: All existing tests PASS (no regressions)

- [ ] **Step 5: Commit**

```bash
git add carina-core/src/parser/mod.rs
git commit -m "feat: add source/version fields to ProviderConfig for pluggable providers"
```

---

## Task 6: Wire `carina-plugin-host` into `carina-cli`

Update the CLI's wiring to spawn provider processes when `source` is specified in a provider block.

**Files:**
- Modify: `carina-cli/Cargo.toml`
- Modify: `carina-cli/src/wiring.rs`

- [ ] **Step 1: Add `carina-plugin-host` dependency to CLI**

In `carina-cli/Cargo.toml`, add to `[dependencies]`:

```toml
carina-plugin-host = { path = "../carina-plugin-host" }
```

- [ ] **Step 2: Update `get_provider_with_ctx` to handle process providers**

In `carina-cli/src/wiring.rs`, modify `get_provider_with_ctx` (around line 357). Add process provider loading for configs with `source`:

```rust
pub async fn get_provider_with_ctx(ctx: &WiringContext, parsed: &ParsedFile) -> ProviderRouter {
    let mut router = ProviderRouter::new();

    for provider_config in &parsed.providers {
        // If the provider has a source, load it as an external process
        if let Some(ref source) = provider_config.source {
            match load_process_provider(source, provider_config).await {
                Ok((provider, name)) => {
                    router.add_provider(name, provider);
                }
                Err(e) => {
                    eprintln!(
                        "Failed to load process provider '{}': {}",
                        provider_config.name, e
                    );
                }
            }
            continue;
        }

        // Otherwise, use the hardcoded factory lookup (existing behavior)
        if let Some(factory) = provider_mod::find_factory(ctx.factories(), &provider_config.name) {
            let region = factory.extract_region(&provider_config.attributes);
            let provider = factory.create_provider(&provider_config.attributes).await;
            router.add_provider(provider_config.name.clone(), provider);
            if let Some(ext) = factory.create_normalizer(&provider_config.attributes).await {
                router.add_normalizer(ext);
            }
        }
    }

    if router.is_empty() {
        router.add_provider(String::new(), Box::new(MockProvider::new()));
    }

    router
}

async fn load_process_provider(
    source: &str,
    config: &carina_core::parser::ProviderConfig,
) -> Result<(Box<dyn carina_core::provider::Provider>, String), String> {
    let binary_path = if source.starts_with("file://") {
        std::path::PathBuf::from(source.strip_prefix("file://").unwrap())
    } else {
        // TODO: Phase 4 will implement download from GitHub Releases.
        // For now, only file:// sources are supported.
        return Err(format!(
            "Remote sources not yet supported. Use file:// for local binaries. Got: {source}"
        ));
    };

    if !binary_path.exists() {
        return Err(format!(
            "Provider binary not found: {}",
            binary_path.display()
        ));
    }

    let factory = carina_plugin_host::ProcessProviderFactory::new(binary_path)?;
    let name = factory.name().to_string();

    factory
        .validate_config(&config.attributes)
        .map_err(|e| format!("Config validation failed: {e}"))?;

    let provider = factory.create_provider(&config.attributes).await;
    Ok((provider, name))
}
```

- [ ] **Step 3: Build and verify**

Run: `cargo build -p carina-cli`
Expected: BUILD SUCCESS

- [ ] **Step 4: Commit**

```bash
git add carina-cli/Cargo.toml carina-cli/src/wiring.rs
git commit -m "feat: wire carina-plugin-host into CLI for external process provider loading"
```

---

## Task 7: E2E integration test — Mock process provider through the host

Write an integration test that builds the mock process provider binary, loads it via `ProcessProviderFactory`, and verifies the full CRUD cycle.

**Files:**
- Create: `carina-plugin-host/tests/mock_process_integration.rs`

- [ ] **Step 1: Write integration test**

```rust
//! Integration test: spawn mock provider process and verify CRUD operations.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use carina_provider_protocol::methods;
use carina_plugin_host::process::ProviderProcess;

fn build_mock_process() -> PathBuf {
    let status = Command::new("cargo")
        .args(["build", "-p", "carina-provider-mock-process"])
        .status()
        .expect("Failed to run cargo build");
    assert!(status.success(), "Failed to build mock-process provider");

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let binary = workspace_root.join("target/debug/carina-provider-mock-process");
    assert!(binary.exists(), "Binary not found: {}", binary.display());
    binary
}

#[test]
fn test_process_provider_info() {
    let binary = build_mock_process();
    let mut process = ProviderProcess::spawn(&binary).expect("Failed to spawn");

    let result: methods::ProviderInfoResult = process
        .call("provider_info", &serde_json::json!({}))
        .expect("provider_info failed");

    assert_eq!(result.info.name, "mock");
    assert_eq!(result.info.display_name, "Mock Provider (Process)");

    process.shutdown();
}

#[test]
fn test_process_provider_crud_cycle() {
    let binary = build_mock_process();
    let mut process = ProviderProcess::spawn(&binary).expect("Failed to spawn");

    // Initialize
    let _: methods::InitializeResult = process
        .call(
            "initialize",
            &methods::InitializeParams {
                attributes: HashMap::new(),
            },
        )
        .expect("initialize failed");

    // Read — should not exist
    let read_result: methods::ReadResult = process
        .call(
            "read",
            &methods::ReadParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: None,
            },
        )
        .expect("read failed");
    assert!(!read_result.state.exists);

    // Create
    let create_result: methods::CreateResult = process
        .call(
            "create",
            &methods::CreateParams {
                resource: carina_provider_protocol::types::Resource {
                    id: carina_provider_protocol::types::ResourceId {
                        provider: "mock".into(),
                        resource_type: "test.resource".into(),
                        name: "hello".into(),
                    },
                    attributes: HashMap::from([(
                        "value".into(),
                        carina_provider_protocol::types::Value::String("world".into()),
                    )]),
                    lifecycle: Default::default(),
                },
            },
        )
        .expect("create failed");
    assert!(create_result.state.exists);
    assert_eq!(create_result.state.identifier, Some("mock-id".into()));

    // Read — should exist now
    let read_result2: methods::ReadResult = process
        .call(
            "read",
            &methods::ReadParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: Some("mock-id".into()),
            },
        )
        .expect("read failed");
    assert!(read_result2.state.exists);

    // Delete
    let delete_result: methods::DeleteResult = process
        .call(
            "delete",
            &methods::DeleteParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: "mock-id".into(),
                lifecycle: Default::default(),
            },
        )
        .expect("delete failed");
    assert!(delete_result.ok);

    // Read — should not exist after delete
    let read_result3: methods::ReadResult = process
        .call(
            "read",
            &methods::ReadParams {
                id: carina_provider_protocol::types::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    name: "hello".into(),
                },
                identifier: None,
            },
        )
        .expect("read failed");
    assert!(!read_result3.state.exists);

    process.shutdown();
}
```

- [ ] **Step 2: Run the integration tests**

Run: `cargo test -p carina-plugin-host --test mock_process_integration`
Expected: 2 tests PASS

- [ ] **Step 3: Commit**

```bash
git add carina-plugin-host/tests/
git commit -m "test: add E2E integration tests for process provider CRUD cycle"
```

---

## Task 8: Validate and fix — iterate until E2E works

This task is an explicit "make it work" step. The previous tasks define the structure, but real integration will likely surface issues.

**Files:**
- Potentially modify any file from Tasks 1-7

- [ ] **Step 1: Build all crates**

```bash
cargo build
```

Fix any compilation errors across the workspace.

- [ ] **Step 2: Run all tests**

```bash
cargo test
```

Fix any test failures. Pay attention to:
- JSON serialization format mismatches between SDK and host
- `BufReader`/`BufWriter` buffering issues (ensure `flush()` is called)
- Process lifecycle (ready message format, shutdown handling)

- [ ] **Step 3: Run the E2E integration test**

```bash
cargo test -p carina-plugin-host --test mock_process_integration
```

Debug and fix any host-guest communication issues.

- [ ] **Step 4: Manual smoke test with CLI**

Build the mock-process binary, then test with a `.crn` file:

```bash
cargo build -p carina-provider-mock-process
```

Create a temp file `/tmp/test-process-provider.crn`:
```crn
provider mock {
    source = "file://TARGET_DIR/debug/carina-provider-mock-process"
}

mock.test.resource {
    name = "hello"
    value = "world"
}
```

Replace `TARGET_DIR` with the actual `target` directory path, then:

```bash
cargo run --bin carina -- validate /tmp/test-process-provider.crn
```

Expected: validation succeeds (or fails gracefully with a clear error about unknown resource type, which is acceptable for Phase 1).

- [ ] **Step 5: Commit all fixes**

```bash
git add -A
git commit -m "fix: resolve integration issues for external process provider pipeline"
```

---

## Summary

| Task | Description | Key Output |
|------|-------------|------------|
| 1 | `carina-provider-protocol` crate | Serializable types + JSON-RPC format |
| 2 | `carina-plugin-sdk` crate | `CarinaProvider` trait + `run()` JSON-RPC server |
| 3 | `carina-provider-mock-process` | Mock provider as standalone binary |
| 4 | `carina-plugin-host` crate | `ProcessProviderFactory` + `ProcessProvider` |
| 5 | Parser `source`/`version` | `ProviderConfig` extended |
| 6 | CLI wiring | `file://` process provider loading |
| 7 | E2E integration test | Proves the pipeline works |
| 8 | Validate and fix | Everything compiles and passes |

After Phase 1, you will have a working external process plugin infrastructure validated with the Mock provider. Phase 2 (AWSCC migration) can begin as a separate plan — and since the AWSCC provider can use AWS SDK directly, it will be a straightforward port.
