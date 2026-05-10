# Phase 2a: awscc External Process Binary — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert `carina-provider-awscc` into a dual lib+binary crate that works as an external process provider via `file://` source, with full CRUD, normalization, state hydration, default tag merging, and schema transfer.

**Architecture:** Extend the Phase 1 plugin protocol with `hydrate_read_state` and `merge_default_tags` RPC methods. Add schema conversion (proto→core) to the host. Create `ProcessProviderNormalizer` backed by a shared process. Add `main.rs` to `carina-provider-awscc` wrapping existing code with `CarinaProvider` trait. Consolidate `carina-provider-mock-process` into `carina-provider-mock`.

**Tech Stack:** `carina-plugin-sdk`, `carina-plugin-host`, `carina-provider-protocol`, `tokio` (for async→sync bridging in binary), `aws-sdk-cloudcontrol`.

**Spec:** `docs/superpowers/specs/2026-03-31-awscc-external-process-design.md`

---

## File Structure

### New Files

```
carina-provider-awscc/src/main.rs          — CarinaProvider wrapper + carina_plugin_sdk::run()
carina-provider-mock/src/main.rs            — CarinaProvider wrapper (moved from mock-process)
carina-plugin-host/src/normalizer.rs        — ProcessProviderNormalizer impl
```

### Modified Files

```
carina-provider-protocol/src/methods.rs     — Add hydrate_read_state + merge_default_tags RPC types
carina-plugin-sdk/src/lib.rs                — Add trait methods + dispatch handlers
carina-plugin-host/src/convert.rs           — Add schema conversion (proto→core, core→proto)
carina-plugin-host/src/factory.rs           — Shared process, schemas(), create_normalizer()
carina-plugin-host/src/lib.rs               — Export normalizer module
Cargo.toml                                  — Remove carina-provider-mock-process from workspace
carina-provider-mock/Cargo.toml             — Add [[bin]] + carina-plugin-sdk dep
carina-provider-awscc/Cargo.toml            — Add [[bin]] + carina-plugin-sdk + tokio dep
carina-plugin-host/tests/mock_process_integration.rs — Update binary path
```

### Deleted

```
carina-provider-mock-process/               — Entire crate (consolidated into carina-provider-mock)
```

---

## Task 1: Extend protocol with `hydrate_read_state` and `merge_default_tags` RPC types

**Files:**
- Modify: `carina-provider-protocol/src/methods.rs`

- [ ] **Step 1: Add `hydrate_read_state` request/response types**

Add to the end of `carina-provider-protocol/src/methods.rs`:

```rust
// -- hydrate_read_state --

#[derive(Debug, Serialize, Deserialize)]
pub struct HydrateReadStateParams {
    pub states: HashMap<String, State>,
    pub saved_attrs: HashMap<String, HashMap<String, Value>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HydrateReadStateResult {
    pub states: HashMap<String, State>,
}
```

- [ ] **Step 2: Add `merge_default_tags` request/response types**

Add to the end of `carina-provider-protocol/src/methods.rs`:

```rust
// -- merge_default_tags --

#[derive(Debug, Serialize, Deserialize)]
pub struct MergeDefaultTagsParams {
    pub resources: Vec<Resource>,
    pub default_tags: HashMap<String, Value>,
    pub schemas: Vec<ResourceSchema>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MergeDefaultTagsResult {
    pub resources: Vec<Resource>,
}
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p carina-provider-protocol`
Expected: BUILD SUCCESS

- [ ] **Step 4: Commit**

```bash
git add carina-provider-protocol/src/methods.rs
git commit -m "feat: add hydrate_read_state and merge_default_tags RPC types to protocol"
```

---

## Task 2: Extend `CarinaProvider` trait and SDK dispatch

**Files:**
- Modify: `carina-plugin-sdk/src/lib.rs`

- [ ] **Step 1: Add `hydrate_read_state` and `merge_default_tags` to `CarinaProvider` trait**

In `carina-plugin-sdk/src/lib.rs`, add these methods to the `CarinaProvider` trait (after `normalize_state`):

```rust
    /// Hydrate read state with saved attributes that APIs don't return.
    fn hydrate_read_state(
        &self,
        states: &mut HashMap<String, State>,
        saved_attrs: &HashMap<String, HashMap<String, Value>>,
    ) {
        let _ = (states, saved_attrs);
    }

    /// Merge provider default_tags into resources.
    fn merge_default_tags(
        &self,
        resources: &mut Vec<Resource>,
        default_tags: &HashMap<String, Value>,
        schemas: &Vec<ResourceSchema>,
    ) {
        let _ = (resources, default_tags, schemas);
    }
```

- [ ] **Step 2: Add dispatch handlers for the new methods**

In the `dispatch` function in `carina-plugin-sdk/src/lib.rs`, add these two match arms before `"shutdown"`:

```rust
        "hydrate_read_state" => {
            let params: methods::HydrateReadStateParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let mut states = params.states;
            provider.hydrate_read_state(&mut states, &params.saved_attrs);
            Response::success(id, methods::HydrateReadStateResult { states })
        }

        "merge_default_tags" => {
            let params: methods::MergeDefaultTagsParams = match parse_params(&request.params) {
                Ok(p) => p,
                Err(e) => return Response::error(id, -32602, e),
            };
            let mut resources = params.resources;
            provider.merge_default_tags(&mut resources, &params.default_tags, &params.schemas);
            Response::success(id, methods::MergeDefaultTagsResult { resources })
        }
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p carina-plugin-sdk`
Expected: BUILD SUCCESS

- [ ] **Step 4: Commit**

```bash
git add carina-plugin-sdk/src/lib.rs
git commit -m "feat: add hydrate_read_state and merge_default_tags to CarinaProvider trait and dispatch"
```

---

## Task 3: Add schema conversion to `carina-plugin-host/src/convert.rs`

The host needs to convert between `proto::ResourceSchema` and `core::ResourceSchema` for schema transfer.

**Files:**
- Modify: `carina-plugin-host/src/convert.rs`

- [ ] **Step 1: Add `proto_to_core_attribute_type` conversion**

Add to `carina-plugin-host/src/convert.rs`:

```rust
use carina_core::schema::{
    AttributeSchema as CoreAttributeSchema, AttributeType as CoreAttributeType,
    ResourceSchema as CoreResourceSchema, StructField as CoreStructField,
};
use carina_provider_protocol::types::{
    AttributeSchema as ProtoAttributeSchema, AttributeType as ProtoAttributeType,
    ResourceSchema as ProtoResourceSchema, StructField as ProtoStructField,
};

fn proto_to_core_attribute_type(t: &ProtoAttributeType) -> CoreAttributeType {
    match t {
        ProtoAttributeType::String => CoreAttributeType::String,
        ProtoAttributeType::Int => CoreAttributeType::Int,
        ProtoAttributeType::Float => CoreAttributeType::Float,
        ProtoAttributeType::Bool => CoreAttributeType::Bool,
        ProtoAttributeType::StringEnum { values } => CoreAttributeType::StringEnum {
            name: String::new(),
            values: values.clone(),
            namespace: None,
            to_dsl: None,
        },
        ProtoAttributeType::List { inner } => CoreAttributeType::List {
            inner: Box::new(proto_to_core_attribute_type(inner)),
            ordered: true,
        },
        ProtoAttributeType::Map { inner } => {
            CoreAttributeType::Map(Box::new(proto_to_core_attribute_type(inner)))
        }
        ProtoAttributeType::Struct { name, fields } => CoreAttributeType::Struct {
            name: name.clone(),
            fields: fields.iter().map(proto_to_core_struct_field).collect(),
        },
    }
}

fn proto_to_core_struct_field(f: &ProtoStructField) -> CoreStructField {
    CoreStructField {
        name: f.name.clone(),
        field_type: proto_to_core_attribute_type(&f.field_type),
        required: f.required,
        description: f.description.clone(),
    }
}
```

- [ ] **Step 2: Add `proto_to_core_schema` and `core_to_proto_schema` conversions**

Add to `carina-plugin-host/src/convert.rs`:

```rust
pub fn proto_to_core_schema(s: &ProtoResourceSchema) -> CoreResourceSchema {
    CoreResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), proto_to_core_attribute_schema(v)))
            .collect(),
        description: s.description.clone(),
        validator: None,
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}

fn proto_to_core_attribute_schema(a: &ProtoAttributeSchema) -> CoreAttributeSchema {
    CoreAttributeSchema {
        name: a.name.clone(),
        attr_type: proto_to_core_attribute_type(&a.attr_type),
        required: a.required,
        default: a.default.as_ref().map(|v| proto_to_core_value(v)),
        description: a.description.clone(),
        completions: None,
        provider_name: None,
        create_only: a.create_only,
        read_only: a.read_only,
        removable: None,
        block_name: None,
        write_only: a.write_only,
    }
}

fn core_to_proto_attribute_type(t: &CoreAttributeType) -> ProtoAttributeType {
    match t {
        CoreAttributeType::String => ProtoAttributeType::String,
        CoreAttributeType::Int => ProtoAttributeType::Int,
        CoreAttributeType::Float => ProtoAttributeType::Float,
        CoreAttributeType::Bool => ProtoAttributeType::Bool,
        CoreAttributeType::StringEnum { values, .. } => ProtoAttributeType::StringEnum {
            values: values.clone(),
        },
        CoreAttributeType::List { inner, .. } => ProtoAttributeType::List {
            inner: Box::new(core_to_proto_attribute_type(inner)),
        },
        CoreAttributeType::Map(inner) => ProtoAttributeType::Map {
            inner: Box::new(core_to_proto_attribute_type(inner)),
        },
        CoreAttributeType::Struct { name, fields } => ProtoAttributeType::Struct {
            name: name.clone(),
            fields: fields.iter().map(core_to_proto_struct_field).collect(),
        },
        // Custom → base type, Union → String (best effort across process boundary)
        CoreAttributeType::Custom { base, .. } => core_to_proto_attribute_type(base),
        CoreAttributeType::Union(_) => ProtoAttributeType::String,
    }
}

fn core_to_proto_struct_field(f: &CoreStructField) -> ProtoStructField {
    ProtoStructField {
        name: f.name.clone(),
        field_type: core_to_proto_attribute_type(&f.field_type),
        required: f.required,
        description: f.description.clone(),
    }
}

pub fn core_to_proto_schema(s: &CoreResourceSchema) -> ProtoResourceSchema {
    ProtoResourceSchema {
        resource_type: s.resource_type.clone(),
        attributes: s
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), core_to_proto_attribute_schema(v)))
            .collect(),
        description: s.description.clone(),
        data_source: s.data_source,
        name_attribute: s.name_attribute.clone(),
        force_replace: s.force_replace,
    }
}

fn core_to_proto_attribute_schema(a: &CoreAttributeSchema) -> ProtoAttributeSchema {
    ProtoAttributeSchema {
        name: a.name.clone(),
        attr_type: core_to_proto_attribute_type(&a.attr_type),
        required: a.required,
        default: a.default.as_ref().map(|v| core_to_proto_value(v)),
        description: a.description.clone(),
        create_only: a.create_only,
        read_only: a.read_only,
        write_only: a.write_only,
    }
}
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p carina-plugin-host`
Expected: BUILD SUCCESS

- [ ] **Step 4: Write schema conversion round-trip test**

Add to `carina-plugin-host/src/convert.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn test_schema_roundtrip() {
        let core_schema = CoreResourceSchema {
            resource_type: "ec2.vpc".into(),
            attributes: HashMap::from([(
                "cidr_block".into(),
                CoreAttributeSchema {
                    name: "cidr_block".into(),
                    attr_type: CoreAttributeType::String,
                    required: true,
                    default: None,
                    description: Some("CIDR block".into()),
                    completions: None,
                    provider_name: None,
                    create_only: true,
                    read_only: false,
                    removable: None,
                    block_name: None,
                    write_only: false,
                },
            )]),
            description: Some("VPC".into()),
            validator: None,
            data_source: false,
            name_attribute: None,
            force_replace: false,
        };

        let proto = core_to_proto_schema(&core_schema);
        let back = proto_to_core_schema(&proto);

        assert_eq!(back.resource_type, "ec2.vpc");
        assert_eq!(back.attributes.len(), 1);
        let attr = &back.attributes["cidr_block"];
        assert_eq!(attr.name, "cidr_block");
        assert!(attr.required);
        assert!(attr.create_only);
        assert_eq!(attr.description, Some("CIDR block".into()));
    }

    #[test]
    fn test_struct_type_roundtrip() {
        let core_type = CoreAttributeType::Struct {
            name: "Tag".into(),
            fields: vec![CoreStructField {
                name: "key".into(),
                field_type: CoreAttributeType::String,
                required: true,
                description: None,
            }],
        };

        let proto = core_to_proto_attribute_type(&core_type);
        let back = proto_to_core_attribute_type(&proto);

        if let CoreAttributeType::Struct { name, fields } = back {
            assert_eq!(name, "Tag");
            assert_eq!(fields.len(), 1);
            assert_eq!(fields[0].name, "key");
        } else {
            panic!("Expected Struct type");
        }
    }
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p carina-plugin-host`
Expected: All tests PASS

- [ ] **Step 6: Commit**

```bash
git add carina-plugin-host/src/convert.rs
git commit -m "feat: add schema conversion between proto and core types"
```

---

## Task 4: Add `ProcessProviderNormalizer`

**Files:**
- Create: `carina-plugin-host/src/normalizer.rs`
- Modify: `carina-plugin-host/src/lib.rs`

- [ ] **Step 1: Create `normalizer.rs`**

Create `carina-plugin-host/src/normalizer.rs`:

```rust
//! ProcessProviderNormalizer forwards normalizer calls to the provider process via JSON-RPC.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use carina_core::provider::{ProviderNormalizer, SavedAttrs};
use carina_core::resource::{Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;
use carina_provider_protocol::methods;

use crate::convert;
use crate::process::ProviderProcess;

pub struct ProcessProviderNormalizer {
    process: Arc<Mutex<ProviderProcess>>,
}

impl ProcessProviderNormalizer {
    pub fn new(process: Arc<Mutex<ProviderProcess>>) -> Self {
        Self { process }
    }
}

impl ProviderNormalizer for ProcessProviderNormalizer {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        let proto_resources: Vec<_> = resources.iter().map(convert::core_to_proto_resource).collect();
        let params = methods::NormalizeDesiredParams {
            resources: proto_resources,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in normalize_desired: {e}");
                return;
            }
        };

        match process.call::<_, methods::NormalizeDesiredResult>("normalize_desired", &params) {
            Ok(result) => {
                // Update resource attributes in-place from the normalized result
                for (core_res, proto_res) in resources.iter_mut().zip(result.resources.iter()) {
                    let resolved = convert::proto_to_core_value_map(&proto_res.attributes);
                    for (key, value) in resolved {
                        core_res.attributes.insert(
                            key,
                            carina_core::resource::Expr(value),
                        );
                    }
                }
            }
            Err(e) => log::error!("normalize_desired RPC failed: {e}"),
        }
    }

    fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) {
        // Convert to proto format (keyed by string)
        let proto_states: HashMap<String, _> = current_states
            .iter()
            .map(|(id, state)| {
                let key = format!("{}.{}.{}", id.provider, id.resource_type, id.name);
                (key, convert::core_to_proto_state(state))
            })
            .collect();

        let params = methods::NormalizeStateParams {
            states: proto_states,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in normalize_state: {e}");
                return;
            }
        };

        match process.call::<_, methods::NormalizeStateResult>("normalize_state", &params) {
            Ok(result) => {
                // Update states in-place
                for state in current_states.values_mut() {
                    let key = format!(
                        "{}.{}.{}",
                        state.id.provider, state.id.resource_type, state.id.name
                    );
                    if let Some(proto_state) = result.states.get(&key) {
                        state.attributes = convert::proto_to_core_value_map(&proto_state.attributes);
                    }
                }
            }
            Err(e) => log::error!("normalize_state RPC failed: {e}"),
        }
    }

    fn hydrate_read_state(
        &self,
        current_states: &mut HashMap<ResourceId, State>,
        saved_attrs: &SavedAttrs,
    ) {
        // Convert states to proto format
        let proto_states: HashMap<String, _> = current_states
            .iter()
            .map(|(id, state)| {
                let key = format!("{}.{}.{}", id.provider, id.resource_type, id.name);
                (key, convert::core_to_proto_state(state))
            })
            .collect();

        // Convert saved_attrs to proto format
        let proto_saved: HashMap<String, HashMap<String, _>> = saved_attrs
            .iter()
            .map(|(id, attrs)| {
                let key = format!("{}.{}.{}", id.provider, id.resource_type, id.name);
                (key, convert::core_to_proto_value_map(attrs))
            })
            .collect();

        let params = methods::HydrateReadStateParams {
            states: proto_states,
            saved_attrs: proto_saved,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in hydrate_read_state: {e}");
                return;
            }
        };

        match process.call::<_, methods::HydrateReadStateResult>("hydrate_read_state", &params) {
            Ok(result) => {
                for state in current_states.values_mut() {
                    let key = format!(
                        "{}.{}.{}",
                        state.id.provider, state.id.resource_type, state.id.name
                    );
                    if let Some(proto_state) = result.states.get(&key) {
                        state.attributes = convert::proto_to_core_value_map(&proto_state.attributes);
                    }
                }
            }
            Err(e) => log::error!("hydrate_read_state RPC failed: {e}"),
        }
    }

    fn merge_default_tags(
        &self,
        resources: &mut [Resource],
        default_tags: &HashMap<String, Value>,
        schemas: &HashMap<String, ResourceSchema>,
    ) {
        let proto_resources: Vec<_> = resources.iter().map(convert::core_to_proto_resource).collect();
        let proto_tags = convert::core_to_proto_value_map(default_tags);
        let proto_schemas: Vec<_> = schemas.values().map(convert::core_to_proto_schema).collect();

        let params = methods::MergeDefaultTagsParams {
            resources: proto_resources,
            default_tags: proto_tags,
            schemas: proto_schemas,
        };

        let mut process = match self.process.lock() {
            Ok(p) => p,
            Err(e) => {
                log::error!("Process lock poisoned in merge_default_tags: {e}");
                return;
            }
        };

        match process.call::<_, methods::MergeDefaultTagsResult>("merge_default_tags", &params) {
            Ok(result) => {
                for (core_res, proto_res) in resources.iter_mut().zip(result.resources.iter()) {
                    let resolved = convert::proto_to_core_value_map(&proto_res.attributes);
                    for (key, value) in resolved {
                        core_res.attributes.insert(
                            key,
                            carina_core::resource::Expr(value),
                        );
                    }
                }
            }
            Err(e) => log::error!("merge_default_tags RPC failed: {e}"),
        }
    }
}
```

- [ ] **Step 2: Export normalizer module in lib.rs**

In `carina-plugin-host/src/lib.rs`, add:

```rust
pub mod normalizer;

pub use normalizer::ProcessProviderNormalizer;
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p carina-plugin-host`
Expected: BUILD SUCCESS

- [ ] **Step 4: Commit**

```bash
git add carina-plugin-host/src/normalizer.rs carina-plugin-host/src/lib.rs
git commit -m "feat: add ProcessProviderNormalizer backed by shared process"
```

---

## Task 5: Refactor `ProcessProviderFactory` for shared process, schemas, and normalizer

**Files:**
- Modify: `carina-plugin-host/src/factory.rs`
- Modify: `carina-plugin-host/src/provider.rs`

- [ ] **Step 1: Change `ProcessProvider` to accept `Arc<Mutex<ProviderProcess>>`**

In `carina-plugin-host/src/provider.rs`, change the struct and constructor:

```rust
use std::sync::{Arc, Mutex};

pub struct ProcessProvider {
    process: Arc<Mutex<ProviderProcess>>,
    name: &'static str,
}

impl ProcessProvider {
    pub fn new(process: Arc<Mutex<ProviderProcess>>, name: String) -> Self {
        let name_static: &'static str = Box::leak(name.into_boxed_str());
        Self {
            process: process,
            name: name_static,
        }
    }
}
```

Update the `lock_process` method accordingly (the return type stays the same since `Arc<Mutex<T>>` has the same `.lock()` interface as `Mutex<T>`).

- [ ] **Step 2: Update `ProcessProviderFactory` to cache schemas and share process**

Rewrite `carina-plugin-host/src/factory.rs`:

```rust
//! ProcessProviderFactory spawns a provider process and implements ProviderFactory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use carina_core::provider::{BoxFuture, Provider, ProviderFactory, ProviderNormalizer};
use carina_core::resource::Value;
use carina_core::schema::ResourceSchema;
use carina_provider_protocol::methods;
use carina_provider_protocol::types::ProviderInfo;

use crate::convert;
use crate::normalizer::ProcessProviderNormalizer;
use crate::process::ProviderProcess;
use crate::provider::ProcessProvider;

pub struct ProcessProviderFactory {
    binary_path: PathBuf,
    info: ProviderInfo,
    schemas: Vec<ResourceSchema>,
    name_static: &'static str,
    display_name_static: &'static str,
}

impl ProcessProviderFactory {
    /// Create a new ProcessProviderFactory by spawning the binary,
    /// querying provider_info and schemas.
    pub fn new(binary_path: PathBuf) -> Result<Self, String> {
        let mut process = ProviderProcess::spawn(&binary_path)?;

        let info_result: methods::ProviderInfoResult = process
            .call("provider_info", &serde_json::json!({}))
            .map_err(|e| format!("Failed to get provider_info: {e}"))?;

        // Fetch schemas from the provider
        let schemas_result: methods::SchemasResult = process
            .call("schemas", &serde_json::json!({}))
            .map_err(|e| format!("Failed to get schemas: {e}"))?;

        let schemas: Vec<ResourceSchema> = schemas_result
            .schemas
            .iter()
            .map(convert::proto_to_core_schema)
            .collect();

        let name_static: &'static str =
            Box::leak(info_result.info.name.clone().into_boxed_str());
        let display_name_static: &'static str =
            Box::leak(info_result.info.display_name.clone().into_boxed_str());

        // Shut down this probe process
        process.shutdown();

        Ok(Self {
            binary_path,
            info: info_result.info,
            schemas,
            name_static,
            display_name_static,
        })
    }

    /// Spawn a provider process, initialize it, and return a shared Arc.
    fn spawn_and_initialize(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> Result<Arc<Mutex<ProviderProcess>>, String> {
        let mut process = ProviderProcess::spawn(&self.binary_path)?;
        let attrs = convert::core_to_proto_value_map(attributes);
        let params = methods::InitializeParams { attributes: attrs };
        let _result: methods::InitializeResult = process
            .call("initialize", &params)
            .map_err(|e| format!("Failed to initialize provider: {e}"))?;
        Ok(Arc::new(Mutex::new(process)))
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
        let attrs = attributes.clone();
        let name = self.info.name.clone();
        Box::pin(async move {
            let process = self
                .spawn_and_initialize(&attrs)
                .expect("Failed to spawn provider process");
            Box::new(ProcessProvider::new(process, name)) as Box<dyn Provider>
        })
    }

    fn create_normalizer(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
        let attrs = attributes.clone();
        Box::pin(async move {
            match self.spawn_and_initialize(&attrs) {
                Ok(process) => {
                    Some(Box::new(ProcessProviderNormalizer::new(process))
                        as Box<dyn ProviderNormalizer>)
                }
                Err(e) => {
                    log::error!("Failed to spawn normalizer process: {e}");
                    None
                }
            }
        })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        self.schemas.clone()
    }
}
```

- [ ] **Step 3: Verify build**

Run: `cargo build -p carina-plugin-host`
Expected: BUILD SUCCESS

- [ ] **Step 4: Run integration tests**

Run: `cargo test -p carina-plugin-host --test mock_process_integration`
Expected: 2 tests PASS

- [ ] **Step 5: Commit**

```bash
git add carina-plugin-host/src/factory.rs carina-plugin-host/src/provider.rs
git commit -m "refactor: shared process in factory, add schemas() and create_normalizer()"
```

---

## Task 6: Consolidate `carina-provider-mock-process` into `carina-provider-mock`

**Files:**
- Modify: `carina-provider-mock/Cargo.toml`
- Create: `carina-provider-mock/src/main.rs`
- Modify: `Cargo.toml` (workspace root)
- Modify: `carina-plugin-host/tests/mock_process_integration.rs`
- Delete: `carina-provider-mock-process/` (entire crate)

- [ ] **Step 1: Add `[[bin]]` and `carina-plugin-sdk` to `carina-provider-mock/Cargo.toml`**

```toml
[package]
name = "carina-provider-mock"
version.workspace = true
edition = "2024"
license = "MIT"
publish = false

[lib]
doctest = false

[[bin]]
name = "carina-provider-mock"
path = "src/main.rs"

[dependencies]
carina-core = { path = "../carina-core" }
carina-plugin-sdk = { path = "../carina-plugin-sdk" }
carina-provider-protocol = { path = "../carina-provider-protocol" }
serde_json = "1"
```

- [ ] **Step 2: Create `carina-provider-mock/src/main.rs`**

```rust
use carina_plugin_sdk::CarinaProvider;
use carina_plugin_sdk::types::*;
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

- [ ] **Step 3: Remove `carina-provider-mock-process` from workspace**

In root `Cargo.toml`, remove `"carina-provider-mock-process"` from the `members` list.

- [ ] **Step 4: Delete `carina-provider-mock-process/` directory**

```bash
rm -rf carina-provider-mock-process/
```

- [ ] **Step 5: Update integration test binary path**

In `carina-plugin-host/tests/mock_process_integration.rs`, change the `build_mock_process` function:

Replace `["build", "-p", "carina-provider-mock-process"]` with `["build", "-p", "carina-provider-mock", "--bin", "carina-provider-mock"]`.

Replace `"target/debug/carina-provider-mock-process"` with `"target/debug/carina-provider-mock"`.

- [ ] **Step 6: Build and test**

Run: `cargo build -p carina-provider-mock`
Run: `cargo test -p carina-plugin-host --test mock_process_integration`
Expected: BUILD SUCCESS, 2 tests PASS

- [ ] **Step 7: Commit**

```bash
git add carina-provider-mock/ carina-plugin-host/tests/ Cargo.toml
git commit -m "refactor: consolidate carina-provider-mock-process into carina-provider-mock"
```

---

## Task 7: Add `main.rs` to `carina-provider-awscc`

**Files:**
- Modify: `carina-provider-awscc/Cargo.toml`
- Create: `carina-provider-awscc/src/main.rs`

- [ ] **Step 1: Add `[[bin]]` section and dependencies to `Cargo.toml`**

Add to `carina-provider-awscc/Cargo.toml`:

```toml
[[bin]]
name = "carina-provider-awscc"
path = "src/main.rs"
```

Add to `[dependencies]`:

```toml
carina-plugin-host = { path = "../carina-plugin-host" }
carina-plugin-sdk = { path = "../carina-plugin-sdk" }
carina-provider-protocol = { path = "../carina-provider-protocol" }
```

Ensure `tokio` has the `rt-multi-thread` and `macros` features (needed for `#[tokio::main]` and `block_on`).

- [ ] **Step 2: Create `carina-provider-awscc/src/main.rs`**

```rust
use std::collections::HashMap;

use carina_plugin_sdk::CarinaProvider;
use carina_provider_protocol::types as proto;

use carina_provider_awscc::provider::AwsccProvider;
use carina_provider_awscc::AwsccNormalizer;
use carina_provider_awscc::schemas;

use carina_core::provider::{Provider, ProviderNormalizer, SavedAttrs};
use carina_core::resource::{ResourceId, State, Value};
use carina_core::schema::ResourceSchema;

struct AwsccProcessProvider {
    runtime: tokio::runtime::Runtime,
    provider: Option<AwsccProvider>,
    normalizer: AwsccNormalizer,
}

impl AwsccProcessProvider {
    fn new() -> Self {
        let runtime = tokio::runtime::Runtime::new().expect("Failed to create tokio runtime");
        Self {
            runtime,
            provider: None,
            normalizer: AwsccNormalizer,
        }
    }

    fn provider(&self) -> &AwsccProvider {
        self.provider
            .as_ref()
            .expect("Provider not initialized — call initialize() first")
    }

    /// Convert proto ResourceId to core ResourceId
    fn to_core_id(id: &proto::ResourceId) -> ResourceId {
        ResourceId::with_provider(&id.provider, &id.resource_type, &id.name)
    }

    /// Convert proto Value to core Value
    fn to_core_value(v: &proto::Value) -> Value {
        match v {
            proto::Value::Bool(b) => Value::Bool(*b),
            proto::Value::Int(i) => Value::Int(*i),
            proto::Value::Float(f) => Value::Float(*f),
            proto::Value::String(s) => Value::String(s.clone()),
            proto::Value::List(l) => Value::List(l.iter().map(Self::to_core_value).collect()),
            proto::Value::Map(m) => Value::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), Self::to_core_value(v)))
                    .collect(),
            ),
        }
    }

    fn to_core_value_map(m: &HashMap<String, proto::Value>) -> HashMap<String, Value> {
        m.iter()
            .map(|(k, v)| (k.clone(), Self::to_core_value(v)))
            .collect()
    }

    /// Convert core Value to proto Value
    fn to_proto_value(v: &Value) -> proto::Value {
        match v {
            Value::Bool(b) => proto::Value::Bool(*b),
            Value::Int(i) => proto::Value::Int(*i),
            Value::Float(f) => proto::Value::Float(*f),
            Value::String(s) => proto::Value::String(s.clone()),
            Value::List(l) => proto::Value::List(l.iter().map(Self::to_proto_value).collect()),
            Value::Map(m) => proto::Value::Map(
                m.iter()
                    .map(|(k, v)| (k.clone(), Self::to_proto_value(v)))
                    .collect(),
            ),
            _ => proto::Value::String(format!("{v:?}")),
        }
    }

    fn to_proto_value_map(m: &HashMap<String, Value>) -> HashMap<String, proto::Value> {
        m.iter()
            .map(|(k, v)| (k.clone(), Self::to_proto_value(v)))
            .collect()
    }

    fn to_proto_state(state: &State) -> proto::State {
        proto::State {
            id: proto::ResourceId {
                provider: state.id.provider.clone(),
                resource_type: state.id.resource_type.clone(),
                name: state.id.name.clone(),
            },
            identifier: state.identifier.clone(),
            attributes: Self::to_proto_value_map(&state.attributes),
            exists: state.exists,
        }
    }

    fn to_core_state(s: &proto::State) -> State {
        let id = Self::to_core_id(&s.id);
        if s.exists {
            let mut state = State::existing(id, Self::to_core_value_map(&s.attributes));
            if let Some(ref ident) = s.identifier {
                state = state.with_identifier(ident);
            }
            state
        } else {
            State::not_found(id)
        }
    }

    /// Convert core ResourceSchema to proto ResourceSchema for the schemas RPC
    fn to_proto_schema(s: &ResourceSchema) -> proto::ResourceSchema {
        carina_plugin_host::convert::core_to_proto_schema(s)
    }
}

impl CarinaProvider for AwsccProcessProvider {
    fn info(&self) -> proto::ProviderInfo {
        proto::ProviderInfo {
            name: "awscc".into(),
            display_name: "AWS Cloud Control provider".into(),
        }
    }

    fn schemas(&self) -> Vec<proto::ResourceSchema> {
        schemas::all_schemas()
            .iter()
            .map(Self::to_proto_schema)
            .collect()
    }

    fn validate_config(&self, attrs: &HashMap<String, proto::Value>) -> Result<(), String> {
        // Validate region if present
        if let Some(proto::Value::String(region)) = attrs.get("region") {
            let _ = carina_core::utils::convert_region_value(region);
        }
        Ok(())
    }

    fn initialize(&mut self, attrs: &HashMap<String, proto::Value>) -> Result<(), String> {
        let region = if let Some(proto::Value::String(r)) = attrs.get("region") {
            carina_core::utils::convert_region_value(r)
        } else {
            "ap-northeast-1".to_string()
        };

        let provider = self.runtime.block_on(AwsccProvider::new(&region));
        self.provider = Some(provider);
        Ok(())
    }

    fn read(
        &self,
        id: &proto::ResourceId,
        identifier: Option<&str>,
    ) -> Result<proto::State, proto::ProviderError> {
        let core_id = Self::to_core_id(id);
        let result = self.runtime.block_on(self.provider().read(&core_id, identifier));
        match result {
            Ok(state) => Ok(Self::to_proto_state(&state)),
            Err(e) => Err(proto::ProviderError {
                message: e.message,
                resource_id: None,
                is_timeout: e.is_timeout,
            }),
        }
    }

    fn create(&self, resource: &proto::Resource) -> Result<proto::State, proto::ProviderError> {
        let core_resource = carina_plugin_host::convert::proto_to_core_resource(resource);
        let result = self.runtime.block_on(self.provider().create(&core_resource));
        match result {
            Ok(state) => Ok(Self::to_proto_state(&state)),
            Err(e) => Err(proto::ProviderError {
                message: e.message,
                resource_id: None,
                is_timeout: e.is_timeout,
            }),
        }
    }

    fn update(
        &self,
        id: &proto::ResourceId,
        identifier: &str,
        from: &proto::State,
        to: &proto::Resource,
    ) -> Result<proto::State, proto::ProviderError> {
        let core_id = Self::to_core_id(id);
        let core_from = Self::to_core_state(from);
        let core_to = carina_plugin_host::convert::proto_to_core_resource(to);
        let result = self
            .runtime
            .block_on(self.provider().update(&core_id, identifier, &core_from, &core_to));
        match result {
            Ok(state) => Ok(Self::to_proto_state(&state)),
            Err(e) => Err(proto::ProviderError {
                message: e.message,
                resource_id: None,
                is_timeout: e.is_timeout,
            }),
        }
    }

    fn delete(
        &self,
        id: &proto::ResourceId,
        identifier: &str,
        lifecycle: &proto::LifecycleConfig,
    ) -> Result<(), proto::ProviderError> {
        let core_id = Self::to_core_id(id);
        let core_lifecycle = carina_core::resource::LifecycleConfig {
            force_delete: lifecycle.force_delete,
            create_before_destroy: lifecycle.create_before_destroy,
        };
        let result = self
            .runtime
            .block_on(self.provider().delete(&core_id, identifier, &core_lifecycle));
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(proto::ProviderError {
                message: e.message,
                resource_id: None,
                is_timeout: e.is_timeout,
            }),
        }
    }

    fn normalize_desired(&self, resources: Vec<proto::Resource>) -> Vec<proto::Resource> {
        let mut core_resources: Vec<_> = resources
            .iter()
            .map(carina_plugin_host::convert::proto_to_core_resource)
            .collect();
        self.normalizer.normalize_desired(&mut core_resources);
        core_resources
            .iter()
            .map(carina_plugin_host::convert::core_to_proto_resource)
            .collect()
    }

    fn normalize_state(
        &self,
        states: HashMap<String, proto::State>,
    ) -> HashMap<String, proto::State> {
        // Convert to core format (keyed by ResourceId)
        let mut core_states: HashMap<ResourceId, State> = states
            .values()
            .map(|s| (Self::to_core_id(&s.id), Self::to_core_state(s)))
            .collect();

        self.normalizer.normalize_state(&mut core_states);

        // Convert back, preserving original keys
        states
            .keys()
            .filter_map(|key| {
                let proto_state = states.get(key)?;
                let core_id = Self::to_core_id(&proto_state.id);
                let updated = core_states.get(&core_id)?;
                Some((key.clone(), Self::to_proto_state(updated)))
            })
            .collect()
    }

    fn hydrate_read_state(
        &self,
        states: &mut HashMap<String, proto::State>,
        saved_attrs: &HashMap<String, HashMap<String, proto::Value>>,
    ) {
        // Convert to core format
        let mut core_states: HashMap<ResourceId, State> = states
            .values()
            .map(|s| (Self::to_core_id(&s.id), Self::to_core_state(s)))
            .collect();

        let core_saved: SavedAttrs = saved_attrs
            .iter()
            .filter_map(|(key, attrs)| {
                let proto_state = states.get(key)?;
                let core_id = Self::to_core_id(&proto_state.id);
                Some((core_id, Self::to_core_value_map(attrs)))
            })
            .collect();

        self.normalizer
            .hydrate_read_state(&mut core_states, &core_saved);

        // Update states in-place
        for (key, proto_state) in states.iter_mut() {
            let core_id = Self::to_core_id(&proto_state.id);
            if let Some(updated) = core_states.get(&core_id) {
                *proto_state = Self::to_proto_state(updated);
            }
        }
    }

    fn merge_default_tags(
        &self,
        resources: &mut Vec<proto::Resource>,
        default_tags: &HashMap<String, proto::Value>,
        schemas: &Vec<proto::ResourceSchema>,
    ) {
        let mut core_resources: Vec<_> = resources
            .iter()
            .map(carina_plugin_host::convert::proto_to_core_resource)
            .collect();

        let core_tags = Self::to_core_value_map(default_tags);
        let core_schemas: HashMap<String, ResourceSchema> = schemas
            .iter()
            .map(|s| {
                (
                    s.resource_type.clone(),
                    carina_plugin_host::convert::proto_to_core_schema(s),
                )
            })
            .collect();

        self.normalizer
            .merge_default_tags(&mut core_resources, &core_tags, &core_schemas);

        // Convert back
        *resources = core_resources
            .iter()
            .map(carina_plugin_host::convert::core_to_proto_resource)
            .collect();
    }
}

fn main() {
    carina_plugin_sdk::run(AwsccProcessProvider::new());
}
```

- [ ] **Step 3: Add `proto_to_core_resource` to `carina-plugin-host/src/convert.rs`**

This function is needed by the awscc binary but is currently missing (Phase 1 only had `core_to_proto_resource`). Add to `carina-plugin-host/src/convert.rs`:

```rust
pub fn proto_to_core_resource(r: &ProtoResource) -> CoreResource {
    use carina_core::resource::Expr;
    let mut resource = CoreResource::with_provider(&r.id.provider, &r.id.resource_type, &r.id.name);
    resource.attributes = r
        .attributes
        .iter()
        .map(|(k, v)| (k.clone(), Expr(proto_to_core_value(v))))
        .collect();
    resource.lifecycle = CoreLifecycle {
        force_delete: r.lifecycle.force_delete,
        create_before_destroy: r.lifecycle.create_before_destroy,
    };
    resource
}
```

- [ ] **Step 4: Build the binary**

Run: `cargo build -p carina-provider-awscc --bin carina-provider-awscc`
Expected: BUILD SUCCESS

- [ ] **Step 5: Smoke test the binary**

```bash
echo '{"jsonrpc":"2.0","id":1,"method":"provider_info","params":{}}
{"jsonrpc":"2.0","id":2,"method":"shutdown","params":{}}' | cargo run -p carina-provider-awscc --bin carina-provider-awscc 2>/dev/null
```

Expected output (3 lines):
```
{"jsonrpc":"2.0","method":"ready"}
{"jsonrpc":"2.0","id":1,"result":{"info":{"name":"awscc","display_name":"AWS Cloud Control provider"}}}
{"jsonrpc":"2.0","id":2,"result":{"ok":true}}
```

- [ ] **Step 6: Commit**

```bash
git add carina-provider-awscc/Cargo.toml carina-provider-awscc/src/main.rs carina-plugin-host/src/convert.rs
git commit -m "feat: add carina-provider-awscc external process binary"
```

---

## Task 8: Validate — full workspace build and test

**Files:**
- Potentially any file from Tasks 1-7

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

- [ ] **Step 3: Run integration tests**

```bash
cargo test -p carina-plugin-host --test mock_process_integration
```

Expected: 2 tests PASS

- [ ] **Step 4: Commit all fixes**

```bash
git add -A
git commit -m "fix: resolve integration issues for awscc external process"
```

---

## Summary

| Task | Description | Key Output |
|------|-------------|------------|
| 1 | Protocol extension | `hydrate_read_state` + `merge_default_tags` RPC types |
| 2 | SDK extension | `CarinaProvider` trait methods + dispatch |
| 3 | Schema conversion | proto ↔ core ResourceSchema conversion |
| 4 | ProcessProviderNormalizer | Normalizer backed by shared process |
| 5 | Factory refactor | Shared process, `schemas()`, `create_normalizer()` |
| 6 | Mock consolidation | `carina-provider-mock-process` → `carina-provider-mock` |
| 7 | awscc binary | `carina-provider-awscc/src/main.rs` with full CRUD + normalizer |
| 8 | Validate | Full workspace build and test |
