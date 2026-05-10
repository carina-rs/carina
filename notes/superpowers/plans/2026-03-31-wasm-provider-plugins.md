# WASM Provider Plugins Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Migrate provider plugins from process-based (JSON-RPC over stdin/stdout) to WebAssembly Component Model with WASI Preview 2, enabling single-file cross-platform distribution.

**Architecture:** Providers compile to `wasm32-wasip2` components exposing a WIT interface. The host (carina-plugin-host) loads them via Wasmtime, providing `wasi:http/outgoing-handler` for network access. During migration, `ProcessProviderFactory` and `WasmProviderFactory` coexist.

**Tech Stack:** Wasmtime (Component Model), WASI Preview 2, wit-bindgen, wasmtime-wasi-http

**Spec:** `docs/superpowers/specs/2026-03-31-wasm-provider-plugins-design.md`

**Repository Structure (updated 2026-04-02):**
- `carina-rs/carina` (this repo): carina-core, carina-cli, carina-plugin-host, carina-plugin-sdk, carina-provider-mock, carina-provider-protocol, carina-lsp, etc.
- `carina-rs/carina-provider-aws` (separate repo): AWS provider binary
- `carina-rs/carina-provider-awscc` (separate repo): AWSCC provider binary

Phase 0-1 work is entirely within the `carina` monorepo (host-side + MockProvider).
Phase 2-3 work happens in the provider repos. `carina-plugin-sdk` and `carina-plugin-wit` are published from `carina` and consumed as dependencies by provider repos.

---

## Phase 0: PoC — AWS SDK wasm32-wasip2 Compilation

This phase is investigative. The goal is to determine whether the AWS SDK for Rust can compile and run under `wasm32-wasip2`. The outcome determines whether to proceed with Phase 1 or fall back to an alternative approach.

### Task 1: Set Up wasm32-wasip2 Toolchain

**Files:**
- No project files modified

- [ ] **Step 1: Install the wasm32-wasip2 target**

```bash
rustup target add wasm32-wasip2
```

Expected: Target installed successfully.

- [ ] **Step 2: Install Wasmtime CLI**

```bash
brew install wasmtime
```

Or via the official installer:

```bash
curl https://wasmtime.dev/install.sh -sSf | bash
```

- [ ] **Step 3: Verify toolchain**

```bash
rustup target list --installed | grep wasm
wasmtime --version
```

Expected: `wasm32-wasip2` listed, wasmtime version printed.

- [ ] **Step 4: Commit — no project changes (toolchain only)**

No commit needed. Toolchain is local.

---

### Task 2: Create Minimal WASM PoC Binary

**Files:**
- Create: `poc-wasm-aws/Cargo.toml`
- Create: `poc-wasm-aws/src/main.rs`

This is a standalone crate outside the workspace to isolate compilation issues.

- [ ] **Step 1: Create the PoC crate**

```bash
mkdir -p /Users/mizzy/src/github.com/carina-rs/poc-wasm-aws
```

Write `poc-wasm-aws/Cargo.toml`:

```toml
[package]
name = "poc-wasm-aws"
version = "0.1.0"
edition = "2024"

[dependencies]
aws-config = { version = "1", features = ["behavior-version-latest"] }
aws-sdk-s3 = "1"
aws-sigv4 = "1"
tokio = { version = "1", features = ["macros", "rt"] }
```

- [ ] **Step 2: Write minimal S3 test code**

Write `poc-wasm-aws/src/main.rs`:

```rust
use aws_sdk_s3::Client as S3Client;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    eprintln!("Starting WASM AWS PoC...");

    let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .region(aws_config::Region::new("ap-northeast-1"))
        .load()
        .await;

    let s3_client = S3Client::new(&config);

    match s3_client.list_buckets().send().await {
        Ok(output) => {
            let buckets = output.buckets();
            eprintln!("Found {} buckets", buckets.len());
            for bucket in buckets {
                eprintln!("  - {}", bucket.name().unwrap_or("unnamed"));
            }
        }
        Err(e) => {
            eprintln!("Error: {e}");
        }
    }
}
```

- [ ] **Step 3: Try compiling to wasm32-wasip2**

```bash
cd /Users/mizzy/src/github.com/carina-rs/poc-wasm-aws
cargo build --target wasm32-wasip2
```

**Expected outcomes (one of):**

A) **Compiles successfully** → Proceed to Step 4.

B) **Fails on crypto crate (`aws-lc-rs` / `ring`)** → Try adding feature flags:

```toml
# Try rustls with ring
aws-config = { version = "1", features = ["behavior-version-latest"], default-features = false }
```

Or investigate `aws-lc-rs` WASM support. Document the specific error.

C) **Fails on tokio** → Try `tokio = { version = "1", features = ["macros", "rt"], default-features = false }` or investigate WASI-compatible async runtimes.

D) **Fails on HTTP client (hyper)** → This is expected. Need to swap the HTTP connector. Investigate `aws_smithy_runtime::client::http` for pluggable connectors.

**Record all errors and blockers** for fallback decision.

- [ ] **Step 4: If compilation succeeds, test with Wasmtime**

```bash
aws-vault exec mizzy -- wasmtime run --wasi http target/wasm32-wasip2/debug/poc-wasm-aws.wasm
```

Note: The `--wasi http` flag enables `wasi:http` support in Wasmtime CLI. The exact flag may vary by Wasmtime version. Check `wasmtime run --help` for WASI HTTP options.

Expected: Buckets listed or a clear HTTP-related error (which confirms compilation works but HTTP adapter needs custom work).

- [ ] **Step 5: Document results**

Create `poc-wasm-aws/RESULTS.md` documenting:
- Whether compilation succeeded
- Which dependencies required changes
- Whether runtime execution worked
- Specific blockers found
- Recommended approach (proceed / F1 / F2 / F3)

---

### Task 3: PoC Decision Point

- [ ] **Step 1: Evaluate results and decide next step**

| Result | Action |
|---|---|
| Compiles & runs | Proceed to Phase 1 |
| Compiles but HTTP needs custom adapter | Proceed to Phase 1 (build WasiHttpClient in Phase 2) |
| Crypto fails but workaround exists | Apply workaround, re-test, then Phase 1 |
| Fundamental blocker | Evaluate F2 (lightweight HTTP) or F3 (schema-only WASM) |

**If proceeding:** Continue with Task 4.
**If blocked:** Create a separate design document for the fallback approach and a new implementation plan.

---

## Phase 1: Foundation — WIT + WasmProviderFactory + MockProvider WASM

### Task 4: Create WIT Interface Definitions

**Files:**
- Create: `carina-plugin-wit/wit/world.wit`
- Create: `carina-plugin-wit/wit/types.wit`
- Create: `carina-plugin-wit/wit/provider.wit`

- [ ] **Step 1: Create the WIT directory**

```bash
mkdir -p /Users/mizzy/src/github.com/carina-rs/carina/carina-plugin-wit/wit
```

- [ ] **Step 2: Write `types.wit`**

Write `carina-plugin-wit/wit/types.wit`:

```wit
interface types {
    record resource-id {
        provider: string,
        resource-type: string,
        name: string,
    }

    variant value {
        bool-val(bool),
        int-val(s64),
        float-val(float64),
        str-val(string),
        list-val(list<value>),
        map-val(list<tuple<string, value>>),
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
        description: option<string>,
        data-source: bool,
        name-attribute: option<string>,
        force-replace: bool,
    }

    record attribute-schema {
        name: string,
        attr-type: attribute-type,
        required: bool,
        description: option<string>,
        create-only: bool,
        read-only: bool,
        write-only: bool,
    }

    variant attribute-type {
        string-type,
        int-type,
        float-type,
        bool-type,
        string-enum(list<string>),
        list-type(attribute-type),
        map-type(attribute-type),
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
        description: option<string>,
    }

    record provider-info {
        name: string,
        display-name: string,
    }
}
```

- [ ] **Step 3: Write `provider.wit`**

Write `carina-plugin-wit/wit/provider.wit`:

```wit
interface provider {
    use types.{
        resource-id, state, resource, lifecycle-config,
        provider-error, resource-schema, value, provider-info,
    };

    info: func() -> provider-info;

    schemas: func() -> list<resource-schema>;

    validate-config: func(attrs: list<tuple<string, value>>) -> result<_, string>;

    initialize: func(attrs: list<tuple<string, value>>) -> result<_, string>;

    read: func(id: resource-id, identifier: option<string>) -> result<state, provider-error>;

    create: func(res: resource) -> result<state, provider-error>;

    update: func(
        id: resource-id,
        identifier: string,
        from: state,
        to: resource,
    ) -> result<state, provider-error>;

    delete: func(
        id: resource-id,
        identifier: string,
        lifecycle: lifecycle-config,
    ) -> result<_, provider-error>;

    normalize-desired: func(resources: list<resource>) -> list<resource>;

    normalize-state: func(
        states: list<tuple<string, state>>,
    ) -> list<tuple<string, state>>;
}
```

- [ ] **Step 4: Write `world.wit`**

Write `carina-plugin-wit/wit/world.wit`:

```wit
package carina:provider@0.1.0;

world carina-provider {
    export provider;
}

world carina-provider-with-http {
    import wasi:http/outgoing-handler@0.2.0;
    export provider;
}
```

Two worlds: `carina-provider` for providers that don't need HTTP (like MockProvider), and `carina-provider-with-http` for providers that make network calls.

- [ ] **Step 5: Validate WIT syntax**

```bash
# Install wasm-tools if needed
cargo install wasm-tools
# Validate
wasm-tools component wit /Users/mizzy/src/github.com/carina-rs/carina/carina-plugin-wit/wit/
```

Expected: No errors. WIT definitions are valid.

- [ ] **Step 6: Commit**

```bash
git add carina-plugin-wit/
git commit -m "feat: add WIT interface definitions for WASM provider plugins"
```

---

### Task 5: Add Wasmtime Dependencies to carina-plugin-host

**Files:**
- Modify: `Cargo.toml` (workspace root — add carina-plugin-wit to members)
- Modify: `carina-plugin-host/Cargo.toml`

- [ ] **Step 1: Add carina-plugin-wit to workspace members**

In workspace root `Cargo.toml`, add `"carina-plugin-wit"` to the `members` list.

Note: `carina-plugin-wit` is not a Rust crate — it's a WIT package. If it doesn't have a `Cargo.toml`, skip adding it to workspace members. The WIT files will be referenced by path from `carina-plugin-host`.

- [ ] **Step 2: Add Wasmtime dependencies to carina-plugin-host**

Add to `carina-plugin-host/Cargo.toml`:

```toml
[dependencies]
wasmtime = { version = "29", features = ["component-model"] }
wasmtime-wasi = "29"
wasmtime-wasi-http = "29"
wit-bindgen = { version = "0.39", default-features = false }
```

Note: Pin to a specific major version. Check the latest compatible versions at build time.

- [ ] **Step 3: Verify it compiles**

```bash
cargo check -p carina-plugin-host
```

Expected: Compiles with warnings about unused imports (new deps not yet used).

- [ ] **Step 4: Generate host-side bindings from WIT**

Add to `carina-plugin-host/src/lib.rs` (or a new `wasm_bindings.rs`):

```rust
wasmtime::component::bindgen!({
    path: "../carina-plugin-wit/wit",
    world: "carina-provider",
    async: true,
});
```

This generates Rust types and traits matching the WIT definitions.

- [ ] **Step 5: Verify bindings compile**

```bash
cargo check -p carina-plugin-host
```

Expected: Compiles. Generated bindings produce types like `carina::provider::types::ResourceId`, `carina::provider::provider::Host`, etc.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml carina-plugin-host/
git commit -m "feat: add Wasmtime dependencies and WIT bindings to carina-plugin-host"
```

---

### Task 6: Implement Core ↔ WIT Type Conversion

**Files:**
- Create: `carina-plugin-host/src/wasm_convert.rs`
- Modify: `carina-plugin-host/src/lib.rs` (add `mod wasm_convert;`)
- Create: `carina-plugin-host/tests/wasm_convert_test.rs`

This module mirrors the existing `convert.rs` (Core ↔ Protocol) but converts between Core types and the Wasmtime-generated WIT types.

- [ ] **Step 1: Write failing tests for Value conversion**

Write `carina-plugin-host/tests/wasm_convert_test.rs`:

```rust
use carina_core::resource::Value as CoreValue;
use carina_plugin_host::wasm_convert;

#[test]
fn test_core_to_wit_value_string() {
    let core = CoreValue::String("hello".into());
    let wit = wasm_convert::core_to_wit_value(&core);
    let roundtrip = wasm_convert::wit_to_core_value(&wit);
    assert_eq!(core, roundtrip);
}

#[test]
fn test_core_to_wit_value_bool() {
    let core = CoreValue::Bool(true);
    let wit = wasm_convert::core_to_wit_value(&core);
    let roundtrip = wasm_convert::wit_to_core_value(&wit);
    assert_eq!(core, roundtrip);
}

#[test]
fn test_core_to_wit_value_int() {
    let core = CoreValue::Int(42);
    let wit = wasm_convert::core_to_wit_value(&core);
    let roundtrip = wasm_convert::wit_to_core_value(&wit);
    assert_eq!(core, roundtrip);
}

#[test]
fn test_core_to_wit_value_float() {
    let core = CoreValue::Float(3.14);
    let wit = wasm_convert::core_to_wit_value(&core);
    let roundtrip = wasm_convert::wit_to_core_value(&wit);
    assert_eq!(core, roundtrip);
}

#[test]
fn test_core_to_wit_value_list() {
    let core = CoreValue::List(vec![
        CoreValue::String("a".into()),
        CoreValue::Int(1),
    ]);
    let wit = wasm_convert::core_to_wit_value(&core);
    let roundtrip = wasm_convert::wit_to_core_value(&wit);
    assert_eq!(core, roundtrip);
}

#[test]
fn test_core_to_wit_value_map() {
    let mut map = std::collections::HashMap::new();
    map.insert("key".to_string(), CoreValue::String("value".into()));
    let core = CoreValue::Map(map);
    let wit = wasm_convert::core_to_wit_value(&core);
    let roundtrip = wasm_convert::wit_to_core_value(&wit);
    assert_eq!(core, roundtrip);
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p carina-plugin-host wasm_convert
```

Expected: FAIL — `wasm_convert` module doesn't exist yet.

- [ ] **Step 3: Implement Value conversion**

Create `carina-plugin-host/src/wasm_convert.rs`:

```rust
use carina_core::resource::Value as CoreValue;
use crate::carina::provider::types::Value as WitValue;

pub fn core_to_wit_value(v: &CoreValue) -> WitValue {
    match v {
        CoreValue::Bool(b) => WitValue::BoolVal(*b),
        CoreValue::Int(i) => WitValue::IntVal(*i),
        CoreValue::Float(f) => WitValue::FloatVal(*f),
        CoreValue::String(s) => WitValue::StrVal(s.clone()),
        CoreValue::List(items) => {
            WitValue::ListVal(items.iter().map(core_to_wit_value).collect())
        }
        CoreValue::Map(map) => {
            WitValue::MapVal(
                map.iter()
                    .map(|(k, v)| (k.clone(), core_to_wit_value(v)))
                    .collect(),
            )
        }
    }
}

pub fn wit_to_core_value(v: &WitValue) -> CoreValue {
    match v {
        WitValue::BoolVal(b) => CoreValue::Bool(*b),
        WitValue::IntVal(i) => CoreValue::Int(*i),
        WitValue::FloatVal(f) => CoreValue::Float(*f),
        WitValue::StrVal(s) => CoreValue::String(s.clone()),
        WitValue::ListVal(items) => {
            CoreValue::List(items.iter().map(wit_to_core_value).collect())
        }
        WitValue::MapVal(entries) => {
            CoreValue::Map(
                entries
                    .iter()
                    .map(|(k, v)| (k.clone(), wit_to_core_value(v)))
                    .collect(),
            )
        }
    }
}
```

Add `pub mod wasm_convert;` to `carina-plugin-host/src/lib.rs`.

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p carina-plugin-host wasm_convert
```

Expected: All 6 tests pass.

- [ ] **Step 5: Write failing tests for ResourceId and State conversion**

Add to `carina-plugin-host/tests/wasm_convert_test.rs`:

```rust
use carina_core::resource::{ResourceId as CoreResourceId, State as CoreState};

#[test]
fn test_resource_id_roundtrip() {
    let core = CoreResourceId {
        provider: "aws".into(),
        resource_type: "s3.bucket".into(),
        name: "my-bucket".into(),
    };
    let wit = wasm_convert::core_to_wit_resource_id(&core);
    let roundtrip = wasm_convert::wit_to_core_resource_id(&wit);
    assert_eq!(core.provider, roundtrip.provider);
    assert_eq!(core.resource_type, roundtrip.resource_type);
    assert_eq!(core.name, roundtrip.name);
}

#[test]
fn test_state_roundtrip() {
    let mut attrs = std::collections::HashMap::new();
    attrs.insert("name".to_string(), CoreValue::String("test".into()));
    let core = CoreState::existing(
        CoreResourceId {
            provider: "mock".into(),
            resource_type: "test.resource".into(),
            name: "res1".into(),
        },
        "id-123",
        attrs,
    );
    let wit = wasm_convert::core_to_wit_state(&core);
    let roundtrip = wasm_convert::wit_to_core_state(&wit);
    assert_eq!(core.identifier, roundtrip.identifier);
    assert_eq!(core.attributes.len(), roundtrip.attributes.len());
}
```

- [ ] **Step 6: Run tests to verify they fail**

```bash
cargo test -p carina-plugin-host wasm_convert
```

Expected: FAIL — functions not defined.

- [ ] **Step 7: Implement ResourceId, State, Resource, LifecycleConfig conversion**

Add to `carina-plugin-host/src/wasm_convert.rs`:

```rust
use carina_core::resource::{
    LifecycleConfig as CoreLifecycle,
    Resource as CoreResource,
    ResourceId as CoreResourceId,
    State as CoreState,
};
use crate::carina::provider::types::{
    LifecycleConfig as WitLifecycle,
    Resource as WitResource,
    ResourceId as WitResourceId,
    State as WitState,
};

pub fn core_to_wit_resource_id(id: &CoreResourceId) -> WitResourceId {
    WitResourceId {
        provider: id.provider.clone(),
        resource_type: id.resource_type.clone(),
        name: id.name.clone(),
    }
}

pub fn wit_to_core_resource_id(id: &WitResourceId) -> CoreResourceId {
    CoreResourceId {
        provider: id.provider.clone(),
        resource_type: id.resource_type.clone(),
        name: id.name.clone(),
    }
}

pub fn core_to_wit_value_map(
    map: &std::collections::HashMap<String, CoreValue>,
) -> Vec<(String, WitValue)> {
    map.iter()
        .map(|(k, v)| (k.clone(), core_to_wit_value(v)))
        .collect()
}

pub fn wit_to_core_value_map(
    entries: &[(String, WitValue)],
) -> std::collections::HashMap<String, CoreValue> {
    entries
        .iter()
        .map(|(k, v)| (k.clone(), wit_to_core_value(v)))
        .collect()
}

pub fn core_to_wit_state(state: &CoreState) -> WitState {
    WitState {
        identifier: state.identifier.clone(),
        attributes: core_to_wit_value_map(&state.attributes),
    }
}

pub fn wit_to_core_state(state: &WitState) -> CoreState {
    CoreState {
        identifier: state.identifier.clone(),
        attributes: wit_to_core_value_map(&state.attributes),
        exists: state.identifier.is_some(),
        id: CoreResourceId {
            provider: String::new(),
            resource_type: String::new(),
            name: String::new(),
        },
    }
}

pub fn core_to_wit_resource(resource: &CoreResource) -> WitResource {
    WitResource {
        id: core_to_wit_resource_id(&resource.id),
        attributes: core_to_wit_value_map(&resource.attributes),
    }
}

pub fn wit_to_core_resource(resource: &WitResource) -> CoreResource {
    CoreResource {
        id: wit_to_core_resource_id(&resource.id),
        attributes: wit_to_core_value_map(&resource.attributes),
        lifecycle: CoreLifecycle::default(),
    }
}

pub fn core_to_wit_lifecycle(lifecycle: &CoreLifecycle) -> WitLifecycle {
    WitLifecycle {
        prevent_destroy: lifecycle.prevent_destroy,
    }
}
```

Note: The exact field names in `CoreState`, `CoreResource`, and `CoreLifecycle` may differ from this code. Check the actual definitions in `carina-core/src/resource.rs` and adjust. The `wit_to_core_state` function creates a placeholder `id` because WIT State doesn't carry the ResourceId — the caller sets it from context.

- [ ] **Step 8: Run tests to verify they pass**

```bash
cargo test -p carina-plugin-host wasm_convert
```

Expected: All 8 tests pass.

- [ ] **Step 9: Write failing tests for Schema conversion**

Add to `carina-plugin-host/tests/wasm_convert_test.rs`:

```rust
use carina_core::schema::{
    AttributeSchema as CoreAttrSchema,
    AttributeType as CoreAttrType,
    ResourceSchema as CoreResSchema,
};

#[test]
fn test_schema_roundtrip_basic() {
    let core = CoreResSchema {
        resource_type: "s3.bucket".to_string(),
        attributes: {
            let mut map = std::collections::HashMap::new();
            map.insert(
                "name".to_string(),
                CoreAttrSchema {
                    name: "name".to_string(),
                    attr_type: CoreAttrType::String,
                    required: true,
                    default: None,
                    description: Some("Bucket name".to_string()),
                    completions: None,
                    provider_name: None,
                    create_only: false,
                    read_only: false,
                    removable: None,
                    block_name: None,
                    write_only: false,
                },
            );
            map
        },
        description: Some("S3 bucket".to_string()),
        validator: None,
        data_source: false,
        name_attribute: None,
        force_replace: false,
    };
    let wit = wasm_convert::core_to_wit_schema(&core);
    let roundtrip = wasm_convert::wit_to_core_schema(&wit);
    assert_eq!(core.resource_type, roundtrip.resource_type);
    assert_eq!(core.description, roundtrip.description);
    assert_eq!(core.attributes.len(), roundtrip.attributes.len());
    assert!(roundtrip.attributes.contains_key("name"));
    assert_eq!(
        roundtrip.attributes["name"].required,
        core.attributes["name"].required,
    );
}
```

- [ ] **Step 10: Run test to verify it fails**

```bash
cargo test -p carina-plugin-host test_schema_roundtrip
```

Expected: FAIL — functions not defined.

- [ ] **Step 11: Implement Schema conversion**

Add to `carina-plugin-host/src/wasm_convert.rs`:

```rust
use carina_core::schema::{
    AttributeSchema as CoreAttrSchema,
    AttributeType as CoreAttrType,
    ResourceSchema as CoreResSchema,
    StructField as CoreStructField,
};
use crate::carina::provider::types::{
    AttributeSchema as WitAttrSchema,
    AttributeType as WitAttrType,
    ResourceSchema as WitResSchema,
    StructDef as WitStructDef,
    StructField as WitStructField,
};

pub fn core_to_wit_attribute_type(t: &CoreAttrType) -> WitAttrType {
    match t {
        CoreAttrType::String => WitAttrType::StringType,
        CoreAttrType::Int => WitAttrType::IntType,
        CoreAttrType::Float => WitAttrType::FloatType,
        CoreAttrType::Bool => WitAttrType::BoolType,
        CoreAttrType::StringEnum { values, .. } => {
            WitAttrType::StringEnum(values.clone())
        }
        CoreAttrType::List { inner, .. } => {
            WitAttrType::ListType(core_to_wit_attribute_type(inner))
        }
        CoreAttrType::Map(inner) => {
            WitAttrType::MapType(core_to_wit_attribute_type(inner))
        }
        CoreAttrType::Struct { name, fields } => {
            WitAttrType::StructType(WitStructDef {
                name: name.clone(),
                fields: fields.iter().map(core_to_wit_struct_field).collect(),
            })
        }
        // Custom and Union don't have WIT equivalents; degrade to base type
        CoreAttrType::Custom { base, .. } => core_to_wit_attribute_type(base),
        CoreAttrType::Union(_) => WitAttrType::StringType,
    }
}

pub fn wit_to_core_attribute_type(t: &WitAttrType) -> CoreAttrType {
    match t {
        WitAttrType::StringType => CoreAttrType::String,
        WitAttrType::IntType => CoreAttrType::Int,
        WitAttrType::FloatType => CoreAttrType::Float,
        WitAttrType::BoolType => CoreAttrType::Bool,
        WitAttrType::StringEnum(values) => CoreAttrType::StringEnum {
            name: String::new(),
            values: values.clone(),
            namespace: None,
            to_dsl: None,
        },
        WitAttrType::ListType(inner) => CoreAttrType::List {
            inner: Box::new(wit_to_core_attribute_type(inner)),
            ordered: true,
        },
        WitAttrType::MapType(inner) => {
            CoreAttrType::Map(Box::new(wit_to_core_attribute_type(inner)))
        }
        WitAttrType::StructType(def) => CoreAttrType::Struct {
            name: def.name.clone(),
            fields: def.fields.iter().map(wit_to_core_struct_field).collect(),
        },
    }
}

fn core_to_wit_struct_field(f: &CoreStructField) -> WitStructField {
    WitStructField {
        name: f.name.clone(),
        field_type: core_to_wit_attribute_type(&f.field_type),
        required: f.required,
        description: f.description.clone(),
    }
}

fn wit_to_core_struct_field(f: &WitStructField) -> CoreStructField {
    CoreStructField {
        name: f.name.clone(),
        field_type: wit_to_core_attribute_type(&f.field_type),
        required: f.required,
        description: f.description.clone(),
        provider_name: None,
        block_name: None,
    }
}

pub fn core_to_wit_schema(s: &CoreResSchema) -> WitResSchema {
    WitResSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .values()
            .map(|a| WitAttrSchema {
                name: a.name.clone(),
                attr_type: core_to_wit_attribute_type(&a.attr_type),
                required: a.required,
                description: a.description.clone(),
                create_only: a.create_only,
                read_only: a.read_only,
                write_only: a.write_only,
            })
            .collect(),
        description: s.description.clone(),
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}

pub fn wit_to_core_schema(s: &WitResSchema) -> CoreResSchema {
    CoreResSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|a| {
                (
                    a.name.clone(),
                    CoreAttrSchema {
                        name: a.name.clone(),
                        attr_type: wit_to_core_attribute_type(&a.attr_type),
                        required: a.required,
                        default: None,
                        description: a.description.clone(),
                        completions: None,
                        provider_name: None,
                        create_only: a.create_only,
                        read_only: a.read_only,
                        removable: None,
                        block_name: None,
                        write_only: a.write_only,
                    },
                )
            })
            .collect(),
        description: s.description.clone(),
        validator: None,
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}
```

- [ ] **Step 12: Run tests to verify they pass**

```bash
cargo test -p carina-plugin-host wasm_convert
```

Expected: All 9 tests pass.

- [ ] **Step 13: Commit**

```bash
git add carina-plugin-host/src/wasm_convert.rs carina-plugin-host/tests/wasm_convert_test.rs carina-plugin-host/src/lib.rs
git commit -m "feat: add Core to WIT type conversion for WASM providers"
```

---

### Task 7: Implement WasmProviderFactory

**Files:**
- Create: `carina-plugin-host/src/wasm_factory.rs`
- Modify: `carina-plugin-host/src/lib.rs` (add `pub mod wasm_factory;`)

- [ ] **Step 1: Write the WasmProviderFactory struct and constructor**

Create `carina-plugin-host/src/wasm_factory.rs`:

```rust
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use carina_core::provider::{
    BoxFuture, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{ResourceId, State, Resource, LifecycleConfig, Value};
use carina_core::schema::ResourceSchema;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, ResourceTable};

use crate::wasm_convert;

// Re-export the generated bindings
use crate::carina::provider as wit_provider;

pub struct WasmProviderFactory {
    engine: Engine,
    component: Component,
    provider_name: &'static str,
    display_name: &'static str,
    schemas: Vec<ResourceSchema>,
}

struct HostState {
    wasi_ctx: WasiCtx,
    table: ResourceTable,
}

impl wasmtime_wasi::WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi_ctx
    }
    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl WasmProviderFactory {
    pub fn from_file(wasm_path: &Path) -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine = Engine::new(&config).map_err(|e| format!("Engine error: {e}"))?;
        let component = Component::from_file(&engine, wasm_path)
            .map_err(|e| format!("Component load error: {e}"))?;

        // Create a temporary instance to get provider info and schemas
        let mut linker = Linker::<HostState>::new(&engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .map_err(|e| format!("WASI linker error: {e}"))?;

        let wasi_ctx = WasiCtxBuilder::new().inherit_stderr().build();
        let mut store = Store::new(
            &engine,
            HostState {
                wasi_ctx,
                table: ResourceTable::new(),
            },
        );

        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(|e| format!("Instantiate error: {e}"))?;

        // Call info() to get provider name
        // Call schemas() to get resource schemas
        // The exact API depends on wasmtime::component::bindgen! output
        // This will be adjusted once bindings are generated

        let info = Self::call_info(&instance, &mut store)?;
        let wit_schemas = Self::call_schemas(&instance, &mut store)?;

        let schemas = wit_schemas
            .iter()
            .map(|s| wasm_convert::wit_to_core_schema(s))
            .collect();

        let provider_name: &'static str = Box::leak(info.name.into_boxed_str());
        let display_name: &'static str = Box::leak(info.display_name.into_boxed_str());

        Ok(Self {
            engine,
            component,
            provider_name,
            display_name,
            schemas,
        })
    }

    fn call_info(
        instance: &wasmtime::component::Instance,
        store: &mut Store<HostState>,
    ) -> Result<wit_provider::types::ProviderInfo, String> {
        // Call the exported info() function
        // Exact API depends on bindgen output — adjust after Step 5 of Task 5
        todo!("Implement after bindings are generated")
    }

    fn call_schemas(
        instance: &wasmtime::component::Instance,
        store: &mut Store<HostState>,
    ) -> Result<Vec<wit_provider::types::ResourceSchema>, String> {
        todo!("Implement after bindings are generated")
    }

    pub fn create_instance(
        &self,
        env_vars: Vec<(String, String)>,
    ) -> Result<(Store<HostState>, wasmtime::component::Instance), String> {
        let mut linker = Linker::<HostState>::new(&self.engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .map_err(|e| format!("WASI linker error: {e}"))?;

        let mut wasi_builder = WasiCtxBuilder::new();
        wasi_builder.inherit_stderr();
        for (key, value) in &env_vars {
            wasi_builder.env(key, value);
        }
        let wasi_ctx = wasi_builder.build();

        let mut store = Store::new(
            &self.engine,
            HostState {
                wasi_ctx,
                table: ResourceTable::new(),
            },
        );

        let instance = linker
            .instantiate(&mut store, &self.component)
            .map_err(|e| format!("Instantiate error: {e}"))?;

        Ok((store, instance))
    }
}
```

Note: The `call_info` and `call_schemas` methods use `todo!()` because the exact API surface generated by `wasmtime::component::bindgen!` depends on the WIT structure. These will be filled in once the bindings are actually generated and inspected. The important thing is the overall structure.

- [ ] **Step 2: Implement ProviderFactory trait**

Add to `carina-plugin-host/src/wasm_factory.rs`:

```rust
impl ProviderFactory for WasmProviderFactory {
    fn name(&self) -> &str {
        self.provider_name
    }

    fn display_name(&self) -> &str {
        self.display_name
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let (mut store, instance) = self.create_instance(vec![])?;
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes);
        // Call validate_config via instance
        // Exact API depends on bindgen output
        todo!("Implement after bindings are generated")
    }

    fn extract_region(&self, attributes: &HashMap<String, Value>) -> String {
        if let Some(Value::String(region)) = attributes.get("region") {
            return carina_core::utils::convert_region_value(region);
        }
        "ap-northeast-1".to_string()
    }

    fn create_provider(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn Provider>> {
        let attrs = attributes.clone();
        Box::pin(async move {
            let env_vars = Self::extract_env_vars(&attrs);
            let (store, instance) = self
                .create_instance(env_vars)
                .expect("Failed to create WASM instance");

            let wit_attrs = wasm_convert::core_to_wit_value_map(&attrs);
            // Call initialize via instance
            // Return WasmProvider wrapping the store and instance

            Box::new(WasmProvider {
                store: Mutex::new(store),
                instance,
                name: self.provider_name,
            }) as Box<dyn Provider>
        })
    }

    fn create_normalizer(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
        let attrs = attributes.clone();
        Box::pin(async move {
            let env_vars = Self::extract_env_vars(&attrs);
            let (store, instance) = self
                .create_instance(env_vars)
                .expect("Failed to create WASM instance for normalizer");

            let wit_attrs = wasm_convert::core_to_wit_value_map(&attrs);
            // Call initialize via instance

            Some(Box::new(WasmProviderNormalizer {
                store: Mutex::new(store),
                instance,
            }) as Box<dyn ProviderNormalizer>)
        })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        self.schemas.clone()
    }
}

impl WasmProviderFactory {
    fn extract_env_vars(attrs: &HashMap<String, Value>) -> Vec<(String, String)> {
        // Extract AWS credential env vars from the current process environment
        let mut env_vars = vec![];
        for key in [
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
        ] {
            if let Ok(val) = std::env::var(key) {
                env_vars.push((key.to_string(), val));
            }
        }
        // Also pass region from attributes
        if let Some(Value::String(region)) = attrs.get("region") {
            let aws_region = carina_core::utils::convert_region_value(region);
            env_vars.push(("AWS_REGION".to_string(), aws_region));
        }
        env_vars
    }
}
```

- [ ] **Step 3: Add WasmProvider struct (placeholder)**

Add to `carina-plugin-host/src/wasm_factory.rs`:

```rust
pub struct WasmProvider {
    store: Mutex<Store<HostState>>,
    instance: wasmtime::component::Instance,
    name: &'static str,
}

impl Provider for WasmProvider {
    fn name(&self) -> &'static str {
        self.name
    }

    fn read(
        &self,
        id: &ResourceId,
        identifier: Option<&str>,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let ident = identifier.map(|s| s.to_string());
        Box::pin(async move {
            let mut store = self.store.lock().unwrap();
            // Call read via instance with wit_id and ident
            // Convert WIT result to Core State
            todo!("Implement after bindings are generated")
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_resource = wasm_convert::core_to_wit_resource(resource);
        Box::pin(async move {
            let mut store = self.store.lock().unwrap();
            todo!("Implement after bindings are generated")
        })
    }

    fn update(
        &self,
        id: &ResourceId,
        identifier: &str,
        from: &State,
        to: &Resource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let ident = identifier.to_string();
        let wit_from = wasm_convert::core_to_wit_state(from);
        let wit_to = wasm_convert::core_to_wit_resource(to);
        Box::pin(async move {
            let mut store = self.store.lock().unwrap();
            todo!("Implement after bindings are generated")
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let ident = identifier.to_string();
        let wit_lifecycle = wasm_convert::core_to_wit_lifecycle(lifecycle);
        Box::pin(async move {
            let mut store = self.store.lock().unwrap();
            todo!("Implement after bindings are generated")
        })
    }
}

pub struct WasmProviderNormalizer {
    store: Mutex<Store<HostState>>,
    instance: wasmtime::component::Instance,
}

impl ProviderNormalizer for WasmProviderNormalizer {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        let wit_resources: Vec<_> = resources
            .iter()
            .map(|r| wasm_convert::core_to_wit_resource(r))
            .collect();
        let mut store = self.store.lock().unwrap();
        // Call normalize_desired via instance
        // Copy results back to resources
        todo!("Implement after bindings are generated")
    }

    fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) {
        let wit_states: Vec<_> = current_states
            .iter()
            .map(|(id, state)| (id.to_string(), wasm_convert::core_to_wit_state(state)))
            .collect();
        let mut store = self.store.lock().unwrap();
        // Call normalize_state via instance
        // Copy results back to current_states
        todo!("Implement after bindings are generated")
    }
}
```

Add `pub mod wasm_factory;` to `carina-plugin-host/src/lib.rs`.

- [ ] **Step 4: Verify it compiles**

```bash
cargo check -p carina-plugin-host
```

Expected: Compiles (with warnings about `todo!()` unreachable code). The `todo!()` calls will be filled in after the actual bindings are inspected in the next step.

- [ ] **Step 5: Fill in bindgen-dependent code**

After Step 4 of Task 5 generates bindings, inspect the generated API:

```bash
# Use cargo-expand or rust-analyzer to see generated types
cargo doc -p carina-plugin-host --document-private-items --open
```

Replace all `todo!("Implement after bindings are generated")` with actual bindgen API calls. The exact names depend on `wasmtime::component::bindgen!` output for the WIT definitions.

Typical pattern:

```rust
// The bindgen! macro generates something like:
// carina::provider::provider::Provider trait (for guest exports)
// And typed accessor functions on the instance

// Example for calling info():
let provider_iface = CarinaProvider::instantiate(&mut store, &component, &linker)?;
let info = provider_iface.provider().call_info(&mut store)?;
```

- [ ] **Step 6: Verify it compiles after filling in bindgen code**

```bash
cargo check -p carina-plugin-host
```

Expected: Compiles without `todo!()` warnings.

- [ ] **Step 7: Commit**

```bash
git add carina-plugin-host/
git commit -m "feat: add WasmProviderFactory with Provider and Normalizer implementations"
```

---

### Task 8: Update carina-plugin-sdk for WASM Guest Support

**Files:**
- Modify: `carina-plugin-sdk/Cargo.toml`
- Create: `carina-plugin-sdk/src/wasm_guest.rs`
- Modify: `carina-plugin-sdk/src/lib.rs`

**Note:** Since provider repos (`carina-provider-aws`, `carina-provider-awscc`) are now separate repositories, `carina-plugin-sdk` and `carina-plugin-wit` must be publishable/consumable as external dependencies (via crates.io or git dependency). The WIT files should be bundled into `carina-plugin-sdk` (e.g., included via `include_str!` or shipped as part of the crate) so that external provider repos don't need a relative path to `carina-plugin-wit/wit/`.

- [ ] **Step 1: Add wit-bindgen dependency**

Add to `carina-plugin-sdk/Cargo.toml`:

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
wit-bindgen = "0.39"
```

- [ ] **Step 2: Create WASM guest module**

Create `carina-plugin-sdk/src/wasm_guest.rs`:

```rust
//! WASM guest-side bindings for CarinaProvider.
//!
//! Provider authors use the `export_provider!` macro to bridge
//! their `CarinaProvider` implementation to the WIT export interface.

// Generate guest bindings from WIT
// WIT files are bundled in the carina-plugin-wit crate.
// For in-monorepo builds, use relative path.
// For external provider repos, this path must resolve via the crate's published content.
wit_bindgen::generate!({
    path: "../carina-plugin-wit/wit",
    world: "carina-provider",
});

use crate::CarinaProvider;

/// Macro to export a CarinaProvider implementation as a WASM component.
///
/// Usage:
/// ```rust
/// struct MyProvider;
/// impl CarinaProvider for MyProvider { ... }
/// carina_plugin_sdk::export_provider!(MyProvider);
/// ```
#[macro_export]
macro_rules! export_provider {
    ($provider_type:ty) => {
        // This macro generates the glue code that bridges
        // CarinaProvider trait methods to WIT export functions.
        //
        // The exact implementation depends on wit-bindgen's generated
        // Guest trait. Typically:
        //
        // struct GuestImpl;
        // impl Guest for GuestImpl {
        //     fn info() -> ProviderInfo { ... }
        //     fn read(id: ResourceId, identifier: Option<String>) -> Result<State, ProviderError> { ... }
        //     ...
        // }
        // export!(GuestImpl);

        static PROVIDER: std::sync::OnceLock<std::sync::Mutex<$provider_type>> =
            std::sync::OnceLock::new();

        fn get_provider() -> &'static std::sync::Mutex<$provider_type> {
            PROVIDER.get_or_init(|| std::sync::Mutex::new(<$provider_type>::default()))
        }

        // The actual trait impl will be filled in after inspecting
        // wit-bindgen's generated Guest trait structure
    };
}
```

- [ ] **Step 3: Add conditional compilation to lib.rs**

Add to `carina-plugin-sdk/src/lib.rs`:

```rust
#[cfg(target_arch = "wasm32")]
pub mod wasm_guest;
```

- [ ] **Step 4: Verify it compiles for native target**

```bash
cargo check -p carina-plugin-sdk
```

Expected: Compiles (wasm_guest module is cfg-gated, not compiled for native).

- [ ] **Step 5: Commit**

```bash
git add carina-plugin-sdk/
git commit -m "feat: add WASM guest support to carina-plugin-sdk"
```

---

### Task 9: Compile MockProvider to WASM

**Files:**
- Modify: `carina-provider-mock/Cargo.toml`
- Create: `carina-provider-mock/src/wasm_main.rs` (or modify `main.rs` with cfg)

- [ ] **Step 1: Add WASM guest dependency to MockProvider**

Add to `carina-provider-mock/Cargo.toml`:

```toml
[target.'cfg(target_arch = "wasm32")'.dependencies]
carina-plugin-sdk = { path = "../carina-plugin-sdk" }
```

- [ ] **Step 2: Add WASM entry point**

Modify `carina-provider-mock/src/main.rs` to support both targets:

```rust
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // Existing JSON-RPC process entry point
    carina_plugin_sdk::run(MockProcessProvider::default());
}

#[cfg(target_arch = "wasm32")]
carina_plugin_sdk::export_provider!(MockProcessProvider);
```

- [ ] **Step 3: Try compiling to wasm32-wasip2**

```bash
cargo build -p carina-provider-mock --target wasm32-wasip2
```

Expected: Either compiles or surfaces dependency issues to fix. MockProvider is simple (no AWS SDK, no network calls) so it should compile.

- [ ] **Step 4: Fix any compilation issues**

Address any errors. Common issues:
- `tokio` features not available on WASM → MockProvider shouldn't need tokio
- `std::fs` not available in WASI → MockProvider uses file-based state; may need to cfg-gate this or use in-memory state for WASM

If MockProvider's file-based state is problematic, create an in-memory-only variant for the WASM build:

```rust
#[cfg(target_arch = "wasm32")]
struct MockProcessProvider {
    states: std::sync::Mutex<HashMap<String, HashMap<String, Value>>>,
}
// In-memory only, no file I/O
```

- [ ] **Step 5: Verify the WASM binary is produced**

```bash
ls -la target/wasm32-wasip2/debug/carina_provider_mock.wasm
```

Expected: File exists.

- [ ] **Step 6: Commit**

```bash
git add carina-provider-mock/
git commit -m "feat: compile MockProvider to wasm32-wasip2"
```

---

### Task 10: Integration Test — MockProvider WASM CRUD

**Files:**
- Create: `carina-plugin-host/tests/wasm_integration_test.rs`

- [ ] **Step 1: Write integration test**

Create `carina-plugin-host/tests/wasm_integration_test.rs`:

```rust
use std::collections::HashMap;
use carina_core::resource::{ResourceId, Value};
use carina_core::provider::{Provider, ProviderFactory};
use carina_plugin_host::wasm_factory::WasmProviderFactory;

#[tokio::test]
async fn test_wasm_mock_provider_create_and_read() {
    let wasm_path = std::path::Path::new(
        "../target/wasm32-wasip2/debug/carina_provider_mock.wasm"
    );
    if !wasm_path.exists() {
        eprintln!("Skipping: MockProvider WASM not built. Run: cargo build -p carina-provider-mock --target wasm32-wasip2");
        return;
    }

    let factory = WasmProviderFactory::from_file(wasm_path)
        .expect("Failed to load MockProvider WASM");

    assert_eq!(factory.name(), "mock");

    let provider = factory
        .create_provider(&HashMap::new())
        .await;

    // Create a resource
    let resource = carina_core::resource::Resource {
        id: ResourceId {
            provider: "mock".into(),
            resource_type: "test.item".into(),
            name: "item1".into(),
        },
        attributes: {
            let mut attrs = HashMap::new();
            attrs.insert("name".to_string(), Value::String("test-item".into()));
            attrs.insert("count".to_string(), Value::Int(42));
            attrs
        },
        lifecycle: Default::default(),
    };

    let state = provider.create(&resource).await.expect("Create failed");
    assert!(state.identifier.is_some());
    assert_eq!(
        state.attributes.get("name"),
        Some(&Value::String("test-item".into())),
    );

    // Read it back
    let read_state = provider
        .read(&resource.id, state.identifier.as_deref())
        .await
        .expect("Read failed");
    assert!(read_state.exists);
    assert_eq!(
        read_state.attributes.get("name"),
        Some(&Value::String("test-item".into())),
    );
}

#[tokio::test]
async fn test_wasm_mock_provider_update_and_delete() {
    let wasm_path = std::path::Path::new(
        "../target/wasm32-wasip2/debug/carina_provider_mock.wasm"
    );
    if !wasm_path.exists() {
        return;
    }

    let factory = WasmProviderFactory::from_file(wasm_path)
        .expect("Failed to load MockProvider WASM");
    let provider = factory.create_provider(&HashMap::new()).await;

    let id = ResourceId {
        provider: "mock".into(),
        resource_type: "test.item".into(),
        name: "item2".into(),
    };

    // Create
    let resource = carina_core::resource::Resource {
        id: id.clone(),
        attributes: {
            let mut attrs = HashMap::new();
            attrs.insert("value".to_string(), Value::String("original".into()));
            attrs
        },
        lifecycle: Default::default(),
    };
    let state = provider.create(&resource).await.unwrap();
    let identifier = state.identifier.clone().unwrap();

    // Update
    let updated_resource = carina_core::resource::Resource {
        id: id.clone(),
        attributes: {
            let mut attrs = HashMap::new();
            attrs.insert("value".to_string(), Value::String("updated".into()));
            attrs
        },
        lifecycle: Default::default(),
    };
    let updated_state = provider
        .update(&id, &identifier, &state, &updated_resource)
        .await
        .unwrap();
    assert_eq!(
        updated_state.attributes.get("value"),
        Some(&Value::String("updated".into())),
    );

    // Delete
    provider
        .delete(&id, &identifier, &Default::default())
        .await
        .unwrap();

    // Read should return not found
    let deleted_state = provider.read(&id, Some(&identifier)).await.unwrap();
    assert!(!deleted_state.exists);
}

#[tokio::test]
async fn test_wasm_mock_provider_schemas() {
    let wasm_path = std::path::Path::new(
        "../target/wasm32-wasip2/debug/carina_provider_mock.wasm"
    );
    if !wasm_path.exists() {
        return;
    }

    let factory = WasmProviderFactory::from_file(wasm_path)
        .expect("Failed to load MockProvider WASM");

    // MockProvider has no schemas, but the call should succeed
    let schemas = factory.schemas();
    // schemas may be empty for mock — that's fine
    assert!(schemas.is_empty() || !schemas.is_empty()); // just verifying no crash
}
```

- [ ] **Step 2: Build MockProvider WASM and run tests**

```bash
cargo build -p carina-provider-mock --target wasm32-wasip2
cargo test -p carina-plugin-host wasm_integration
```

Expected: All 3 tests pass.

- [ ] **Step 3: Commit**

```bash
git add carina-plugin-host/tests/wasm_integration_test.rs
git commit -m "test: add WASM MockProvider integration tests"
```

---

### Task 11: Wire WasmProviderFactory into CLI

**Files:**
- Modify: `carina-cli/src/wiring.rs`
- Modify: `carina-cli/src/provider_resolver.rs`

- [ ] **Step 1: Update provider_resolver to handle .wasm files**

In `carina-cli/src/provider_resolver.rs`, update `resolve_asset_name` (or equivalent) to detect WASM:

```rust
pub fn is_wasm_provider(path: &Path) -> bool {
    path.extension().map_or(false, |ext| ext == "wasm")
}
```

- [ ] **Step 2: Update build_factories_from_providers in wiring.rs**

In `carina-cli/src/wiring.rs`, update the factory creation logic:

```rust
use carina_plugin_host::wasm_factory::WasmProviderFactory;

// In build_factories_from_providers(), after resolving the binary path:
let factory: Box<dyn ProviderFactory> = if provider_resolver::is_wasm_provider(&binary_path) {
    Box::new(WasmProviderFactory::from_file(&binary_path)?)
} else {
    Box::new(ProcessProviderFactory::new(binary_path)?)
};
```

- [ ] **Step 3: Verify it compiles**

```bash
cargo check -p carina-cli
```

Expected: Compiles.

- [ ] **Step 4: Commit**

```bash
git add carina-cli/src/wiring.rs carina-cli/src/provider_resolver.rs
git commit -m "feat: wire WasmProviderFactory into CLI provider loading"
```

---

### Task 12: Precompile Cache

**Files:**
- Modify: `carina-plugin-host/src/wasm_factory.rs`
- Create: `carina-plugin-host/tests/wasm_precompile_test.rs`

- [ ] **Step 1: Write failing test for precompile cache**

Create `carina-plugin-host/tests/wasm_precompile_test.rs`:

```rust
use carina_plugin_host::wasm_factory::WasmProviderFactory;
use std::path::PathBuf;

#[test]
fn test_precompile_cache_creation() {
    let wasm_path = PathBuf::from("../target/wasm32-wasip2/debug/carina_provider_mock.wasm");
    if !wasm_path.exists() {
        return;
    }

    let cache_dir = tempfile::tempdir().unwrap();
    let cwasm_path = cache_dir.path().join("mock.cwasm");

    // First load: should create cache
    WasmProviderFactory::precompile(&wasm_path, &cwasm_path)
        .expect("Precompile failed");
    assert!(cwasm_path.exists());

    // Second load: should use cache
    let factory = WasmProviderFactory::from_precompiled(&cwasm_path)
        .expect("Load from cache failed");
    assert_eq!(factory.name(), "mock");
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test -p carina-plugin-host wasm_precompile
```

Expected: FAIL — `precompile` and `from_precompiled` don't exist.

- [ ] **Step 3: Implement precompile cache**

Add to `carina-plugin-host/src/wasm_factory.rs`:

```rust
impl WasmProviderFactory {
    pub fn precompile(wasm_path: &Path, cwasm_path: &Path) -> Result<(), String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine = Engine::new(&config).map_err(|e| format!("Engine error: {e}"))?;
        let wasm_bytes = std::fs::read(wasm_path)
            .map_err(|e| format!("Read error: {e}"))?;
        let serialized = engine
            .precompile_component(&wasm_bytes)
            .map_err(|e| format!("Precompile error: {e}"))?;

        if let Some(parent) = cwasm_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Mkdir error: {e}"))?;
        }
        std::fs::write(cwasm_path, &serialized)
            .map_err(|e| format!("Write error: {e}"))?;

        Ok(())
    }

    pub fn from_precompiled(cwasm_path: &Path) -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);

        let engine = Engine::new(&config).map_err(|e| format!("Engine error: {e}"))?;
        let component = unsafe {
            Component::deserialize_file(&engine, cwasm_path)
                .map_err(|e| format!("Deserialize error: {e}"))?
        };

        // Same initialization as from_file but using precompiled component
        let mut linker = Linker::<HostState>::new(&engine);
        wasmtime_wasi::add_to_linker_sync(&mut linker)
            .map_err(|e| format!("WASI linker error: {e}"))?;

        let wasi_ctx = WasiCtxBuilder::new().inherit_stderr().build();
        let mut store = Store::new(
            &engine,
            HostState {
                wasi_ctx,
                table: ResourceTable::new(),
            },
        );

        let instance = linker
            .instantiate(&mut store, &component)
            .map_err(|e| format!("Instantiate error: {e}"))?;

        let info = Self::call_info(&instance, &mut store)?;
        let wit_schemas = Self::call_schemas(&instance, &mut store)?;
        let schemas = wit_schemas
            .iter()
            .map(|s| wasm_convert::wit_to_core_schema(s))
            .collect();

        let provider_name: &'static str = Box::leak(info.name.into_boxed_str());
        let display_name: &'static str = Box::leak(info.display_name.into_boxed_str());

        Ok(Self {
            engine,
            component,
            provider_name,
            display_name,
            schemas,
        })
    }

    /// Load from .wasm with optional precompile cache.
    pub fn from_file_cached(wasm_path: &Path, cache_dir: &Path) -> Result<Self, String> {
        let stem = wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("provider");
        let cwasm_path = cache_dir.join(format!("{stem}.cwasm"));

        if cwasm_path.exists() {
            match Self::from_precompiled(&cwasm_path) {
                Ok(factory) => return Ok(factory),
                Err(e) => {
                    // Cache may be stale (Wasmtime version changed)
                    eprintln!("Precompile cache invalid, recompiling: {e}");
                    let _ = std::fs::remove_file(&cwasm_path);
                }
            }
        }

        Self::precompile(wasm_path, &cwasm_path)?;
        Self::from_precompiled(&cwasm_path)
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test -p carina-plugin-host wasm_precompile
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add carina-plugin-host/
git commit -m "feat: add precompile (AOT) cache for WASM providers"
```

---

## Phase 2-5: Outline (Separate Plans)

Phase 2-5 depend on the outcome of Phase 0 (PoC) and the specific bindgen API surface discovered in Phase 1. Each phase should get its own detailed plan when the time comes.

**Important: Provider repos are now separate repositories.** Phase 2-3 work happens in `carina-rs/carina-provider-aws` and `carina-rs/carina-provider-awscc` respectively, not in this monorepo.

### Phase 2: AWS Provider WASM Compilation

**Repo:** `carina-rs/carina-provider-aws`

Prerequisites from Phase 1 (monorepo side):
- `carina-plugin-sdk` with WASM guest support must be published/available as a git dependency
- `carina-plugin-wit` WIT files must be bundled or accessible from the SDK

Work in the provider repo:
- Add `carina-plugin-sdk` dependency (git or crates.io)
- Implement `WasiHttpClient` in `carina-plugin-sdk` (implements `aws_smithy_runtime_api::client::http::HttpClient`) — or in a shared crate if both providers need it
- Swap AWS SDK HTTP connector to `WasiHttpClient` via `#[cfg(target_arch = "wasm32")]`
- Add `export_provider!(AwsProcessProvider)` with cfg-gate in `src/main.rs`
- Compile to `wasm32-wasip2` and fix any remaining dependency issues
- Run existing provider tests against WASM build
- E2E test: `plan` and `apply` with WASM provider
- Update CI to build `.wasm` artifacts alongside native binaries

### Phase 3: AWSCC Provider WASM Compilation

**Repo:** `carina-rs/carina-provider-awscc`

- Same approach as Phase 2
- AWSCC uses CloudControl API (single SDK) — may be simpler than AWS provider
- Shares `WasiHttpClient` from `carina-plugin-sdk`

### Phase 4: Distribution

**Repos:** All three (`carina`, `carina-provider-aws`, `carina-provider-awscc`)

Monorepo (`carina`) side:
- Update `provider_resolver.rs` to download `.wasm` from releases
- Remove OS/arch detection for WASM providers (single file download)
- Add precompile cache to the download/resolve flow
- Update SHA256 verification for `.wasm` files

Provider repos side:
- Update GitHub Actions CI to build and release `.wasm` artifacts
- Update release workflows to publish single `.wasm` + SHA256 instead of OS/arch binaries

### Phase 5: Cleanup

**Repos:** All three

Monorepo (`carina`) side:
- Remove `ProcessProviderFactory`, `ProcessProvider`, `ProcessProviderNormalizer` from `carina-plugin-host`
- Remove `carina-provider-protocol` crate from workspace
- Remove `ProviderProcess` (stdin/stdout JSON-RPC)
- Remove `jsonrpc.rs` and `methods.rs`

Provider repos side:
- Remove JSON-RPC `run()` entry point from `main.rs`
- Remove `carina-provider-protocol` dependency
- Keep only WASM `export_provider!()` entry point
