//! WasmProviderFactory loads a WASM component and implements ProviderFactory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tokio::sync::Mutex;
use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpView};

use carina_core::provider::{
    BoxFuture, Provider, ProviderError, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{Expr, LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;

use crate::wasm_bindings::CarinaProvider;
use crate::wasm_bindings_http::CarinaProviderWithHttp;
use crate::wasm_convert;

// -- Host state for WASI --

struct HostState {
    wasi_ctx: WasiCtx,
    http_ctx: Option<WasiHttpCtx>,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> &mut WasiCtx {
        &mut self.wasi_ctx
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

impl WasiHttpView for HostState {
    fn ctx(&mut self) -> &mut WasiHttpCtx {
        self.http_ctx
            .as_mut()
            .expect("HTTP not enabled for this provider")
    }

    fn table(&mut self) -> &mut ResourceTable {
        &mut self.table
    }
}

// -- Helper: create a new Store + CarinaProvider instance --

/// Bindings enum wrapping both non-HTTP and HTTP WASM component bindings.
/// Both worlds export the same `carina:provider/provider` interface.
enum WasmBindings {
    Basic(CarinaProvider),
    Http(CarinaProviderWithHttp),
}

use crate::wasm_bindings::carina::provider::types as wit_types;

impl WasmBindings {
    async fn call_info(
        &self,
        store: &mut Store<HostState>,
    ) -> wasmtime::Result<wit_types::ProviderInfo> {
        match self {
            WasmBindings::Basic(b) => b.carina_provider_provider().call_info(store).await,
            WasmBindings::Http(b) => b.carina_provider_provider().call_info(store).await,
        }
    }

    async fn call_schemas(
        &self,
        store: &mut Store<HostState>,
    ) -> wasmtime::Result<Vec<wit_types::ResourceSchema>> {
        match self {
            WasmBindings::Basic(b) => b.carina_provider_provider().call_schemas(store).await,
            WasmBindings::Http(b) => b.carina_provider_provider().call_schemas(store).await,
        }
    }

    async fn call_validate_config(
        &self,
        store: &mut Store<HostState>,
        attrs: &[(String, wit_types::Value)],
    ) -> wasmtime::Result<Result<(), String>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_validate_config(store, attrs)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_validate_config(store, attrs)
                    .await
            }
        }
    }

    async fn call_initialize(
        &self,
        store: &mut Store<HostState>,
        attrs: &[(String, wit_types::Value)],
    ) -> wasmtime::Result<Result<(), String>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_initialize(store, attrs)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_initialize(store, attrs)
                    .await
            }
        }
    }

    async fn call_read(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: Option<&str>,
    ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_read(store, id, identifier)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_read(store, id, identifier)
                    .await
            }
        }
    }

    async fn call_create(
        &self,
        store: &mut Store<HostState>,
        resource: &wit_types::ResourceDef,
    ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_create(store, resource)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_create(store, resource)
                    .await
            }
        }
    }

    async fn call_update(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: &str,
        from: &wit_types::State,
        to: &wit_types::ResourceDef,
    ) -> wasmtime::Result<Result<wit_types::State, wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_update(store, id, identifier, from, to)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_update(store, id, identifier, from, to)
                    .await
            }
        }
    }

    async fn call_delete(
        &self,
        store: &mut Store<HostState>,
        id: &wit_types::ResourceId,
        identifier: &str,
        lifecycle: wit_types::LifecycleConfig,
    ) -> wasmtime::Result<Result<(), wit_types::ProviderError>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_delete(store, id, identifier, lifecycle)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_delete(store, id, identifier, lifecycle)
                    .await
            }
        }
    }

    async fn call_normalize_desired(
        &self,
        store: &mut Store<HostState>,
        resources: &[wit_types::ResourceDef],
    ) -> wasmtime::Result<Vec<wit_types::ResourceDef>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_normalize_desired(store, resources)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_normalize_desired(store, resources)
                    .await
            }
        }
    }

    async fn call_normalize_state(
        &self,
        store: &mut Store<HostState>,
        states: &[(String, wit_types::State)],
    ) -> wasmtime::Result<Vec<(String, wit_types::State)>> {
        match self {
            WasmBindings::Basic(b) => {
                b.carina_provider_provider()
                    .call_normalize_state(store, states)
                    .await
            }
            WasmBindings::Http(b) => {
                b.carina_provider_provider()
                    .call_normalize_state(store, states)
                    .await
            }
        }
    }
}

async fn create_instance(
    engine: &Engine,
    component: &Component,
) -> Result<(Store<HostState>, WasmBindings), String> {
    let wasi_ctx = WasiCtxBuilder::new().inherit_stderr().build();
    let host_state = HostState {
        wasi_ctx,
        http_ctx: None,
        table: ResourceTable::new(),
    };
    let mut store = Store::new(engine, host_state);

    let mut linker = Linker::new(engine);
    wasmtime_wasi::add_to_linker_async(&mut linker)
        .map_err(|e| format!("Failed to add WASI to linker: {e}"))?;

    let bindings = CarinaProvider::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| format!("Failed to instantiate WASM component: {e}"))?;

    Ok((store, WasmBindings::Basic(bindings)))
}

async fn create_instance_with_http(
    engine: &Engine,
    component: &Component,
) -> Result<(Store<HostState>, WasmBindings), String> {
    let wasi_ctx = WasiCtxBuilder::new().inherit_stderr().inherit_env().build();
    let host_state = HostState {
        wasi_ctx,
        http_ctx: Some(WasiHttpCtx::new()),
        table: ResourceTable::new(),
    };
    let mut store = Store::new(engine, host_state);

    let mut linker = Linker::new(engine);
    wasmtime_wasi::add_to_linker_async(&mut linker)
        .map_err(|e| format!("Failed to add WASI to linker: {e}"))?;
    wasmtime_wasi_http::add_only_http_to_linker_async(&mut linker)
        .map_err(|e| format!("Failed to add wasi:http to linker: {e}"))?;

    let bindings = CarinaProviderWithHttp::instantiate_async(&mut store, component, &linker)
        .await
        .map_err(|e| format!("Failed to instantiate WASM component (HTTP): {e}"))?;

    Ok((store, WasmBindings::Http(bindings)))
}

// -- WasmProviderFactory --

pub struct WasmProviderFactory {
    engine: Engine,
    component: Component,
    #[allow(dead_code)]
    wasm_path: PathBuf,
    name_static: &'static str,
    display_name_static: &'static str,
    schemas: Vec<ResourceSchema>,
    enable_http: bool,
}

impl WasmProviderFactory {
    pub async fn new(wasm_path: PathBuf) -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine =
            Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}"))?;

        let component = Component::from_file(&engine, &wasm_path).map_err(|e| {
            format!(
                "Failed to load WASM component from {}: {e}",
                wasm_path.display()
            )
        })?;

        // Detect whether the component needs HTTP by trying HTTP instantiation first,
        // then falling back to basic.
        let (mut store, bindings, enable_http) =
            match create_instance_with_http(&engine, &component).await {
                Ok((store, bindings)) => (store, bindings, true),
                Err(_) => {
                    let (store, bindings) = create_instance(&engine, &component).await?;
                    (store, bindings, false)
                }
            };
        let info = bindings
            .call_info(&mut store)
            .await
            .map_err(|e| format!("Failed to call info(): {e}"))?;

        let wit_schemas = bindings
            .call_schemas(&mut store)
            .await
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let schemas: Vec<ResourceSchema> = wit_schemas
            .iter()
            .map(wasm_convert::wit_to_core_schema)
            .collect();

        let name_static: &'static str = Box::leak(info.name.into_boxed_str());
        let display_name_static: &'static str = Box::leak(info.display_name.into_boxed_str());

        // Drop the temporary instance
        drop(store);

        Ok(Self {
            engine,
            component,
            wasm_path,
            name_static,
            display_name_static,
            schemas,
            enable_http,
        })
    }

    /// Precompile a .wasm file and save the result to a .cwasm file.
    pub fn precompile(wasm_path: &Path, cwasm_path: &Path) -> Result<(), String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine = Engine::new(&config).map_err(|e| format!("Engine error: {e}"))?;
        let wasm_bytes = std::fs::read(wasm_path).map_err(|e| format!("Read error: {e}"))?;
        let serialized = engine
            .precompile_component(&wasm_bytes)
            .map_err(|e| format!("Precompile error: {e}"))?;
        if let Some(parent) = cwasm_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("Mkdir error: {e}"))?;
        }
        std::fs::write(cwasm_path, &serialized).map_err(|e| format!("Write error: {e}"))?;
        Ok(())
    }

    /// Load from a precompiled .cwasm file.
    ///
    /// # Safety
    /// The .cwasm file must have been produced by `precompile()` using the same
    /// Wasmtime version. Deserializing an untrusted or corrupted file is unsafe.
    pub async fn from_precompiled(cwasm_path: &Path) -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine =
            Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}"))?;

        // SAFETY: The caller is responsible for ensuring the .cwasm file was
        // produced by a trusted `precompile()` call with the same Wasmtime version.
        let component =
            unsafe { Component::deserialize_file(&engine, cwasm_path) }.map_err(|e| {
                format!(
                    "Failed to deserialize WASM component from {}: {e}",
                    cwasm_path.display()
                )
            })?;

        let (mut store, bindings, enable_http) =
            match create_instance_with_http(&engine, &component).await {
                Ok((store, bindings)) => (store, bindings, true),
                Err(_) => {
                    let (store, bindings) = create_instance(&engine, &component).await?;
                    (store, bindings, false)
                }
            };
        let info = bindings
            .call_info(&mut store)
            .await
            .map_err(|e| format!("Failed to call info(): {e}"))?;

        let wit_schemas = bindings
            .call_schemas(&mut store)
            .await
            .map_err(|e| format!("Failed to call schemas(): {e}"))?;

        let schemas: Vec<ResourceSchema> = wit_schemas
            .iter()
            .map(wasm_convert::wit_to_core_schema)
            .collect();

        let name_static: &'static str = Box::leak(info.name.into_boxed_str());
        let display_name_static: &'static str = Box::leak(info.display_name.into_boxed_str());

        drop(store);

        Ok(Self {
            engine,
            component,
            wasm_path: cwasm_path.to_path_buf(),
            name_static,
            display_name_static,
            schemas,
            enable_http,
        })
    }

    /// Load from .wasm with automatic precompile caching.
    ///
    /// Checks for an existing `.cwasm` in `cache_dir`. If present, attempts to
    /// load it; if the cache is stale or invalid, recompiles and caches anew.
    pub async fn from_file_cached(wasm_path: &Path, cache_dir: &Path) -> Result<Self, String> {
        let stem = wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("provider");
        let cwasm_path = cache_dir.join(format!("{stem}.cwasm"));

        if cwasm_path.exists() {
            match Self::from_precompiled(&cwasm_path).await {
                Ok(factory) => return Ok(factory),
                Err(e) => {
                    eprintln!("Precompile cache invalid, recompiling: {e}");
                    let _ = std::fs::remove_file(&cwasm_path);
                }
            }
        }

        Self::precompile(wasm_path, &cwasm_path)?;
        Self::from_precompiled(&cwasm_path).await
    }

    async fn create_initialized_instance(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> Result<(Store<HostState>, WasmBindings), String> {
        let (mut store, bindings) = if self.enable_http {
            create_instance_with_http(&self.engine, &self.component).await?
        } else {
            create_instance(&self.engine, &self.component).await?
        };
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes);
        bindings
            .call_initialize(&mut store, &wit_attrs)
            .await
            .map_err(|e| format!("Failed to call initialize(): {e}"))?
            .map_err(|e| format!("Provider initialization failed: {e}"))?;

        Ok((store, bindings))
    }
}

impl ProviderFactory for WasmProviderFactory {
    fn name(&self) -> &str {
        self.name_static
    }

    fn display_name(&self) -> &str {
        self.display_name_static
    }

    fn validate_config(&self, attributes: &HashMap<String, Value>) -> Result<(), String> {
        let engine = self.engine.clone();
        let component = self.component.clone();
        let enable_http = self.enable_http;
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes);

        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let (mut store, bindings) = if enable_http {
                    create_instance_with_http(&engine, &component).await?
                } else {
                    create_instance(&engine, &component).await?
                };
                bindings
                    .call_validate_config(&mut store, &wit_attrs)
                    .await
                    .map_err(|e| format!("Failed to call validate_config(): {e}"))?
            })
        })
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
        Box::pin(async move {
            let (store, bindings) = self
                .create_initialized_instance(&attrs)
                .await
                .expect("Failed to create WASM provider instance");
            Box::new(WasmProvider {
                store: Mutex::new(store),
                bindings,
                name: self.name_static,
            }) as Box<dyn Provider>
        })
    }

    fn create_normalizer(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> BoxFuture<'_, Option<Box<dyn ProviderNormalizer>>> {
        let attrs = attributes.clone();
        Box::pin(async move {
            match self.create_initialized_instance(&attrs).await {
                Ok((store, bindings)) => Some(Box::new(WasmProviderNormalizer {
                    store: Mutex::new(store),
                    bindings,
                }) as Box<dyn ProviderNormalizer>),
                Err(e) => {
                    log::error!("Failed to create WASM normalizer instance: {e}");
                    None
                }
            }
        })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        self.schemas.clone()
    }
}

// -- WasmProvider --

pub struct WasmProvider {
    store: Mutex<Store<HostState>>,
    bindings: WasmBindings,
    name: &'static str,
}

// Safety: The Store is behind a Mutex, so concurrent access is serialized.
// The bindings are only used while the store mutex is held.
unsafe impl Send for WasmProvider {}
unsafe impl Sync for WasmProvider {}

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
        let id = id.clone();
        let identifier = identifier.map(|s| s.to_string());
        Box::pin(async move {
            let mut store = self.store.lock().await;
            let result = self
                .bindings
                .call_read(&mut store, &wit_id, identifier.as_deref())
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in read: {e}")))?;
            match result {
                Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(&wit_err)),
            }
        })
    }

    fn create(&self, resource: &Resource) -> BoxFuture<'_, ProviderResult<State>> {
        let wit_resource = wasm_convert::core_to_wit_resource(resource);
        let id = resource.id.clone();
        Box::pin(async move {
            let mut store = self.store.lock().await;
            let result = self
                .bindings
                .call_create(&mut store, &wit_resource)
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in create: {e}")))?;
            match result {
                Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(&wit_err)),
            }
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
        let identifier = identifier.to_string();
        let wit_from = wasm_convert::core_to_wit_state(from);
        let wit_to = wasm_convert::core_to_wit_resource(to);
        let id = id.clone();
        Box::pin(async move {
            let mut store = self.store.lock().await;
            let result = self
                .bindings
                .call_update(&mut store, &wit_id, &identifier, &wit_from, &wit_to)
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in update: {e}")))?;
            match result {
                Ok(wit_state) => Ok(wasm_convert::wit_to_core_state(&wit_state, &id)),
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(&wit_err)),
            }
        })
    }

    fn delete(
        &self,
        id: &ResourceId,
        identifier: &str,
        lifecycle: &LifecycleConfig,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        let wit_id = wasm_convert::core_to_wit_resource_id(id);
        let identifier = identifier.to_string();
        let wit_lifecycle = wasm_convert::core_to_wit_lifecycle(lifecycle);
        Box::pin(async move {
            let mut store = self.store.lock().await;
            let result = self
                .bindings
                .call_delete(&mut store, &wit_id, &identifier, wit_lifecycle)
                .await
                .map_err(|e| ProviderError::new(format!("WASM trap in delete: {e}")))?;
            match result {
                Ok(()) => Ok(()),
                Err(wit_err) => Err(wasm_convert::wit_to_core_provider_error(&wit_err)),
            }
        })
    }
}

// -- WasmProviderNormalizer --

pub struct WasmProviderNormalizer {
    store: Mutex<Store<HostState>>,
    bindings: WasmBindings,
}

// Safety: Same rationale as WasmProvider.
unsafe impl Send for WasmProviderNormalizer {}
unsafe impl Sync for WasmProviderNormalizer {}

impl ProviderNormalizer for WasmProviderNormalizer {
    fn normalize_desired(&self, resources: &mut [Resource]) {
        let wit_resources: Vec<_> = resources
            .iter()
            .map(wasm_convert::core_to_wit_resource)
            .collect();

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut store = self.store.lock().await;
                self.bindings
                    .call_normalize_desired(&mut store, &wit_resources)
                    .await
            })
        });

        match result {
            Ok(result) => {
                for (core_res, wit_res) in resources.iter_mut().zip(result.iter()) {
                    let resolved = wasm_convert::wit_to_core_value_map(&wit_res.attributes);
                    for (key, value) in resolved {
                        core_res.attributes.insert(key, Expr(value));
                    }
                }
            }
            Err(e) => log::error!("WASM trap in normalize_desired: {e}"),
        }
    }

    fn normalize_state(&self, current_states: &mut HashMap<ResourceId, State>) {
        let wit_states: Vec<(String, _)> = current_states
            .iter()
            .map(|(id, state)| (id.to_string(), wasm_convert::core_to_wit_state(state)))
            .collect();

        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let mut store = self.store.lock().await;
                self.bindings
                    .call_normalize_state(&mut store, &wit_states)
                    .await
            })
        });

        match result {
            Ok(result) => {
                for state in current_states.values_mut() {
                    let key = state.id.to_string();
                    if let Some((_, wit_state)) = result.iter().find(|(k, _)| k == &key) {
                        state.attributes =
                            wasm_convert::wit_to_core_value_map(&wit_state.attributes);
                    }
                }
            }
            Err(e) => log::error!("WASM trap in normalize_state: {e}"),
        }
    }
}
