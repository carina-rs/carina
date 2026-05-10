# Protocol Version Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add protocol version handshake and capability negotiation to the plugin system so that incompatible providers are detected at startup with clear error messages.

**Architecture:** The `ready` notification gains a `protocol_version` field. The host validates it on connection. `ProviderInfo` gains a `capabilities` field listing optional methods the plugin supports. The host skips JSON-RPC calls for unsupported capabilities.

**Tech Stack:** Rust, serde, JSON-RPC 2.0 over stdin/stdout

**Spec:** `docs/superpowers/specs/2026-04-02-protocol-version-compatibility-design.md`

---

### Task 1: Add PROTOCOL_VERSION constant and update ready notification

**Files:**
- Modify: `carina-provider-protocol/src/lib.rs`
- Modify: `carina-provider-protocol/src/jsonrpc.rs`
- Test: `carina-provider-protocol/src/jsonrpc.rs` (inline tests)

- [ ] **Step 1: Write failing test for ready notification with protocol_version**

Add to `carina-provider-protocol/src/jsonrpc.rs` in the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn test_notification_ready_includes_protocol_version() {
    let notif = Notification::ready();
    let json: serde_json::Value = serde_json::to_value(&notif).unwrap();
    let params = json.get("params").expect("ready should have params");
    let version = params
        .get("protocol_version")
        .expect("params should have protocol_version");
    assert_eq!(version, &serde_json::Value::Number(1.into()));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p carina-provider-protocol test_notification_ready_includes_protocol_version`

Expected: FAIL — `Notification::ready()` currently sets `params: None`.

- [ ] **Step 3: Add PROTOCOL_VERSION constant**

In `carina-provider-protocol/src/lib.rs`, add before the `pub use` lines:

```rust
/// Protocol version for host-plugin communication.
/// Increment when making breaking changes to the protocol types or methods.
pub const PROTOCOL_VERSION: u32 = 1;
```

- [ ] **Step 4: Update Notification::ready() to include protocol_version**

In `carina-provider-protocol/src/jsonrpc.rs`, change `Notification::ready()`:

```rust
impl Notification {
    pub fn ready() -> Self {
        Self {
            jsonrpc: "2.0".into(),
            method: "ready".into(),
            params: Some(serde_json::json!({
                "protocol_version": crate::PROTOCOL_VERSION,
            })),
        }
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p carina-provider-protocol`

Expected: All tests pass, including the new one.

- [ ] **Step 6: Commit**

```bash
git add carina-provider-protocol/src/lib.rs carina-provider-protocol/src/jsonrpc.rs
git commit -m "feat: add PROTOCOL_VERSION constant and include in ready notification"
```

---

### Task 2: Add capabilities field to ProviderInfo

**Files:**
- Modify: `carina-provider-protocol/src/types.rs`
- Test: `carina-provider-protocol/src/types.rs` (inline tests)

- [ ] **Step 1: Write failing test for ProviderInfo with capabilities**

Add to `carina-provider-protocol/src/types.rs` in the `#[cfg(test)] mod tests` block:

```rust
#[test]
fn test_provider_info_with_capabilities() {
    let info = ProviderInfo {
        name: "test".into(),
        display_name: "Test Provider".into(),
        capabilities: vec!["normalize_desired".into(), "normalize_state".into()],
    };
    let json = serde_json::to_string(&info).unwrap();
    let back: ProviderInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(back.capabilities, vec!["normalize_desired", "normalize_state"]);
}

#[test]
fn test_provider_info_without_capabilities_defaults_to_empty() {
    // Simulates deserializing a response from an older plugin that doesn't send capabilities
    let json = r#"{"name":"old","display_name":"Old Provider"}"#;
    let info: ProviderInfo = serde_json::from_str(json).unwrap();
    assert!(info.capabilities.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p carina-provider-protocol test_provider_info_with_capabilities test_provider_info_without_capabilities`

Expected: FAIL — `ProviderInfo` struct doesn't have a `capabilities` field.

- [ ] **Step 3: Add capabilities field to ProviderInfo**

In `carina-provider-protocol/src/types.rs`, change the `ProviderInfo` struct:

```rust
/// Provider metadata returned by `provider_info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub display_name: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p carina-provider-protocol`

Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add carina-provider-protocol/src/types.rs
git commit -m "feat: add capabilities field to ProviderInfo"
```

---

### Task 3: Add capabilities method to CarinaProvider trait and update SDK

**Files:**
- Modify: `carina-plugin-sdk/src/lib.rs`

- [ ] **Step 1: Add capabilities() method with default to CarinaProvider trait**

In `carina-plugin-sdk/src/lib.rs`, add to the `CarinaProvider` trait after the `info()` method:

```rust
/// Return the list of optional capabilities this provider supports.
/// Possible values: "normalize_desired", "normalize_state",
/// "hydrate_read_state", "merge_default_tags".
fn capabilities(&self) -> Vec<String> {
    vec![]
}
```

- [ ] **Step 2: Update dispatch to include capabilities in provider_info response**

In `carina-plugin-sdk/src/lib.rs`, change the `"provider_info"` match arm in the `dispatch` function:

```rust
"provider_info" => {
    let mut info = provider.info();
    info.capabilities = provider.capabilities();
    Response::success(id, methods::ProviderInfoResult { info })
}
```

- [ ] **Step 3: Build to verify compilation**

Run: `cargo build -p carina-plugin-sdk`

Expected: Compiles successfully. `capabilities()` has a default implementation so existing providers don't break.

- [ ] **Step 4: Commit**

```bash
git add carina-plugin-sdk/src/lib.rs
git commit -m "feat: add capabilities() to CarinaProvider trait and include in provider_info"
```

---

### Task 4: Validate protocol version in host process spawn

**Files:**
- Modify: `carina-plugin-host/src/process.rs`
- Test: `carina-plugin-host/src/process.rs` (inline tests)

- [ ] **Step 1: Write failing test for protocol version validation**

Add a test module to `carina-plugin-host/src/process.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_protocol_version_matching() {
        let params = serde_json::json!({ "protocol_version": carina_provider_protocol::PROTOCOL_VERSION });
        let result = validate_protocol_version(Some(&params));
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_protocol_version_mismatch() {
        let params = serde_json::json!({ "protocol_version": 999 });
        let result = validate_protocol_version(Some(&params));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("protocol version 999"));
        assert!(err.contains(&format!("version {}", carina_provider_protocol::PROTOCOL_VERSION)));
    }

    #[test]
    fn test_validate_protocol_version_missing_params() {
        // Old plugin that doesn't send protocol_version in ready
        let result = validate_protocol_version(None);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("did not report a protocol version"));
    }

    #[test]
    fn test_validate_protocol_version_missing_field() {
        let params = serde_json::json!({});
        let result = validate_protocol_version(Some(&params));
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p carina-plugin-host test_validate_protocol_version`

Expected: FAIL — `validate_protocol_version` function doesn't exist.

- [ ] **Step 3: Implement validate_protocol_version function**

Add to `carina-plugin-host/src/process.rs` before the `impl ProviderProcess` block:

```rust
fn validate_protocol_version(params: Option<&serde_json::Value>) -> Result<(), String> {
    let expected = carina_provider_protocol::PROTOCOL_VERSION;

    let params = params.ok_or_else(|| {
        format!(
            "Plugin did not report a protocol version. Carina requires protocol version {expected}."
        )
    })?;

    let version = params.get("protocol_version").and_then(|v| v.as_u64());

    match version {
        Some(v) if v as u32 == expected => Ok(()),
        Some(v) => {
            if (v as u32) < expected {
                Err(format!(
                    "Plugin uses protocol version {v}, but Carina requires version {expected}. \
                     Please update the plugin."
                ))
            } else {
                Err(format!(
                    "Plugin uses protocol version {v}, but this version of Carina only supports \
                     version {expected}. Please update Carina."
                ))
            }
        }
        None => Err(format!(
            "Plugin did not report a protocol version. Carina requires protocol version {expected}."
        )),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p carina-plugin-host test_validate_protocol_version`

Expected: All 4 tests pass.

- [ ] **Step 5: Integrate validation into ProviderProcess::spawn**

In `carina-plugin-host/src/process.rs`, update the `spawn` method. Replace the current ready-message check:

```rust
let trimmed = line.trim();
if !trimmed.contains("\"ready\"") {
    return Err(format!("Expected ready notification, got: {trimmed}"));
}
```

With:

```rust
let trimmed = line.trim();
if !trimmed.contains("\"ready\"") {
    return Err(format!("Expected ready notification, got: {trimmed}"));
}

// Validate protocol version from ready notification params
let notification: carina_provider_protocol::jsonrpc::Notification =
    serde_json::from_str(trimmed)
        .map_err(|e| format!("Failed to parse ready notification: {e}"))?;
validate_protocol_version(notification.params.as_ref())?;
```

- [ ] **Step 6: Build and run all plugin-host tests**

Run: `cargo test -p carina-plugin-host`

Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add carina-plugin-host/src/process.rs
git commit -m "feat: validate protocol version on plugin connection"
```

---

### Task 5: Store capabilities in factory and gate normalizer calls

**Files:**
- Modify: `carina-plugin-host/src/factory.rs`
- Modify: `carina-plugin-host/src/normalizer.rs`

- [ ] **Step 1: Store capabilities in ProcessProviderFactory**

In `carina-plugin-host/src/factory.rs`, add `capabilities` field to the struct:

```rust
pub struct ProcessProviderFactory {
    binary_path: PathBuf,
    info: ProviderInfo,
    schemas: Vec<ResourceSchema>,
    name_static: &'static str,
    display_name_static: &'static str,
    capabilities: Vec<String>,
}
```

In `ProcessProviderFactory::new`, after `let schemas_result = ...`, store capabilities and pass to the struct:

```rust
let capabilities = info_result.info.capabilities.clone();
```

Add `capabilities` to the `Ok(Self { ... })` return.

- [ ] **Step 2: Pass capabilities to ProcessProviderNormalizer**

In `carina-plugin-host/src/factory.rs`, change `create_normalizer` to pass capabilities:

```rust
fn create_normalizer(
    &self,
    attributes: &HashMap<String, Value>,
) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
    let attrs = attributes.clone();
    let capabilities = self.capabilities.clone();
    Box::pin(async move {
        match self.spawn_and_initialize(&attrs) {
            Ok(process) => Some(Box::new(ProcessProviderNormalizer::new(process, capabilities))
                as Box<dyn ProviderNormalizer>),
            Err(e) => {
                log::error!("Failed to spawn normalizer process: {e}");
                None
            }
        }
    })
}
```

- [ ] **Step 3: Update ProcessProviderNormalizer to accept and use capabilities**

In `carina-plugin-host/src/normalizer.rs`, update the struct and constructor:

```rust
pub struct ProcessProviderNormalizer {
    process: Arc<Mutex<ProviderProcess>>,
    capabilities: Vec<String>,
}

impl ProcessProviderNormalizer {
    pub fn new(process: Arc<Mutex<ProviderProcess>>, capabilities: Vec<String>) -> Self {
        Self {
            process,
            capabilities,
        }
    }

    fn lock_process(&self) -> Result<MutexGuard<'_, ProviderProcess>, ()> {
        self.process.lock().map_err(|e| {
            log::error!("Process lock poisoned: {e}");
        })
    }

    fn has_capability(&self, cap: &str) -> bool {
        self.capabilities.iter().any(|c| c == cap)
    }
}
```

- [ ] **Step 4: Add capability guards to each normalizer method**

In each `ProviderNormalizer` method implementation in `carina-plugin-host/src/normalizer.rs`, add an early return at the start of the method body:

For `normalize_desired`:
```rust
fn normalize_desired(&self, resources: &mut [Resource]) {
    if !self.has_capability("normalize_desired") {
        return;
    }
    // ... existing code unchanged
}
```

For `normalize_state`:
```rust
fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) {
    if !self.has_capability("normalize_state") {
        return;
    }
    // ... existing code unchanged
}
```

For `hydrate_read_state`:
```rust
fn hydrate_read_state(
    &self,
    current_states: &mut HashMap<ResourceId, State>,
    saved_attrs: &SavedAttrs,
) {
    if !self.has_capability("hydrate_read_state") {
        return;
    }
    // ... existing code unchanged
}
```

For `merge_default_tags`:
```rust
fn merge_default_tags(
    &self,
    resources: &mut [Resource],
    default_tags: &HashMap<String, Value>,
    schemas: &HashMap<String, ResourceSchema>,
) {
    if !self.has_capability("merge_default_tags") {
        return;
    }
    // ... existing code unchanged
}
```

- [ ] **Step 5: Build to verify compilation**

Run: `cargo build -p carina-plugin-host`

Expected: Compiles successfully.

- [ ] **Step 6: Commit**

```bash
git add carina-plugin-host/src/factory.rs carina-plugin-host/src/normalizer.rs
git commit -m "feat: gate normalizer RPC calls on plugin-reported capabilities"
```

---

### Task 6: Update mock provider and SDK ready notification

**Files:**
- Modify: `carina-provider-mock/src/main.rs`
- Modify: `carina-plugin-sdk/src/lib.rs` (ready notification already uses `Notification::ready()` which was updated in Task 1)

- [ ] **Step 1: Verify SDK already sends protocol version in ready**

The `carina-plugin-sdk/src/lib.rs` `run()` function calls `Notification::ready()` which was updated in Task 1 to include `protocol_version`. No change needed here.

Run: `cargo build -p carina-plugin-sdk`

Expected: Compiles successfully.

- [ ] **Step 2: Update mock provider to not declare capabilities (it has none)**

The `MockProcessProvider::info()` currently returns:

```rust
ProviderInfo {
    name: "mock".into(),
    display_name: "Mock Provider (Process)".into(),
}
```

Since `capabilities` defaults to `vec![]` via `#[serde(default)]`, and the mock provider doesn't override `capabilities()` (which defaults to `vec![]` in the trait), no change is needed.

Verify: `cargo build -p carina-provider-mock`

Expected: Compiles successfully.

- [ ] **Step 3: Run full workspace build and test**

Run: `cargo build && cargo test`

Expected: All crates compile. All tests pass.

- [ ] **Step 4: Commit (if any changes were needed)**

If no changes were needed (likely), skip this commit. Otherwise:

```bash
git commit -m "chore: update mock provider for protocol version compatibility"
```

---

### Task 7: Integration test — end-to-end protocol version handshake

**Files:**
- Modify: `carina-plugin-host/Cargo.toml` (add dev-dependency)
- Create: `carina-plugin-host/tests/protocol_version.rs`

- [ ] **Step 1: Add carina-provider-mock as dev-dependency**

In `carina-plugin-host/Cargo.toml`, add a `[dev-dependencies]` section:

```toml
[dev-dependencies]
carina-provider-mock = { path = "../carina-provider-mock" }
serde_json = "1"
```

This is needed so `CARGO_BIN_EXE_carina-provider-mock` is available in integration tests.

- [ ] **Step 2: Write integration test**

Create `carina-plugin-host/tests/protocol_version.rs`:

```rust
//! Integration test: spawn the mock provider binary and verify protocol version handshake.

use std::path::PathBuf;

fn mock_binary_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_carina-provider-mock"));
    assert!(path.exists(), "mock provider binary not found at {path:?}");
    path
}

#[test]
fn test_spawn_mock_provider_succeeds() {
    let path = mock_binary_path();
    let mut process = carina_plugin_host::process::ProviderProcess::spawn(&path)
        .expect("Should spawn mock provider with matching protocol version");
    process.shutdown();
}
```

- [ ] **Step 3: Run the integration test**

Run: `cargo test -p carina-plugin-host --test protocol_version`

Expected: PASS — mock provider sends ready with matching protocol version.

- [ ] **Step 4: Commit**

```bash
git add carina-plugin-host/Cargo.toml carina-plugin-host/tests/protocol_version.rs
git commit -m "test: add integration test for protocol version handshake"
```

---

### Task 8: Final verification

- [ ] **Step 1: Run full workspace tests**

Run: `cargo test`

Expected: All tests pass across all crates.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -- -D warnings`

Expected: No warnings.

- [ ] **Step 3: Verify plan display fixtures still work**

Run: `make plan-fixtures`

Expected: All fixture-based plan displays work correctly (this change shouldn't affect display).
