//! WasmProviderFactory loads a WASM component and implements ProviderFactory.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use wasmtime::component::{Component, Linker};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{ResourceTable, WasiCtx, WasiCtxBuilder, WasiView};

use carina_core::provider::{
    BoxFuture, Provider, ProviderError, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{Expr, LifecycleConfig, Resource, ResourceId, State, Value};
use carina_core::schema::ResourceSchema;

use crate::wasm_bindings::CarinaProvider;
use crate::wasm_convert;

// -- Host state for WASI --

struct HostState {
    wasi_ctx: WasiCtx,
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

// -- Helper: create a new Store + CarinaProvider instance --

fn create_instance(
    engine: &Engine,
    component: &Component,
) -> Result<(Store<HostState>, CarinaProvider), String> {
    let wasi_ctx = WasiCtxBuilder::new().inherit_stderr().build();
    let host_state = HostState {
        wasi_ctx,
        table: ResourceTable::new(),
    };
    let mut store = Store::new(engine, host_state);

    let mut linker = Linker::new(engine);
    wasmtime_wasi::add_to_linker_sync(&mut linker)
        .map_err(|e| format!("Failed to add WASI to linker: {e}"))?;

    let bindings = CarinaProvider::instantiate(&mut store, component, &linker)
        .map_err(|e| format!("Failed to instantiate WASM component: {e}"))?;

    Ok((store, bindings))
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
}

impl WasmProviderFactory {
    pub fn new(wasm_path: PathBuf) -> Result<Self, String> {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        let engine =
            Engine::new(&config).map_err(|e| format!("Failed to create WASM engine: {e}"))?;

        let component = Component::from_file(&engine, &wasm_path).map_err(|e| {
            format!(
                "Failed to load WASM component from {}: {e}",
                wasm_path.display()
            )
        })?;

        // Create a temporary instance to call info() and schemas()
        let (mut store, bindings) = create_instance(&engine, &component)?;
        let guest = bindings.carina_provider_provider();

        let info = guest
            .call_info(&mut store)
            .map_err(|e| format!("Failed to call info(): {e}"))?;

        let wit_schemas = guest
            .call_schemas(&mut store)
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
        })
    }

    fn create_initialized_instance(
        &self,
        attributes: &HashMap<String, Value>,
    ) -> Result<(Store<HostState>, CarinaProvider), String> {
        let (mut store, bindings) = create_instance(&self.engine, &self.component)?;
        let guest = bindings.carina_provider_provider();

        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes);
        guest
            .call_initialize(&mut store, &wit_attrs)
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
        let (mut store, bindings) = create_instance(&self.engine, &self.component)?;
        let guest = bindings.carina_provider_provider();
        let wit_attrs = wasm_convert::core_to_wit_value_map(attributes);
        guest
            .call_validate_config(&mut store, &wit_attrs)
            .map_err(|e| format!("Failed to call validate_config(): {e}"))?
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
            match self.create_initialized_instance(&attrs) {
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
    bindings: CarinaProvider,
    name: &'static str,
}

// Safety: The Store is behind a Mutex, so concurrent access is serialized.
// The CarinaProvider bindings are only used while the store mutex is held.
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
            let mut store = self
                .store
                .lock()
                .map_err(|e| ProviderError::new(format!("Store lock poisoned: {e}")))?;
            let guest = self.bindings.carina_provider_provider();
            let result = guest
                .call_read(&mut *store, &wit_id, identifier.as_deref())
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
            let mut store = self
                .store
                .lock()
                .map_err(|e| ProviderError::new(format!("Store lock poisoned: {e}")))?;
            let guest = self.bindings.carina_provider_provider();
            let result = guest
                .call_create(&mut *store, &wit_resource)
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
            let mut store = self
                .store
                .lock()
                .map_err(|e| ProviderError::new(format!("Store lock poisoned: {e}")))?;
            let guest = self.bindings.carina_provider_provider();
            let result = guest
                .call_update(&mut *store, &wit_id, &identifier, &wit_from, &wit_to)
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
            let mut store = self
                .store
                .lock()
                .map_err(|e| ProviderError::new(format!("Store lock poisoned: {e}")))?;
            let guest = self.bindings.carina_provider_provider();
            let result = guest
                .call_delete(&mut *store, &wit_id, &identifier, wit_lifecycle)
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
    bindings: CarinaProvider,
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

        let Ok(mut store) = self.store.lock() else {
            log::error!("Store lock poisoned in normalize_desired");
            return;
        };

        let guest = self.bindings.carina_provider_provider();
        match guest.call_normalize_desired(&mut *store, &wit_resources) {
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

        let Ok(mut store) = self.store.lock() else {
            log::error!("Store lock poisoned in normalize_state");
            return;
        };

        let guest = self.bindings.carina_provider_provider();
        match guest.call_normalize_state(&mut *store, &wit_states) {
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
