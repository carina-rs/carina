pub mod wasm_convert;
pub mod wasm_factory;

pub mod wasm_bindings {
    wasmtime::component::bindgen!({
        path: "../carina-plugin-wit/wit",
        world: "carina-provider",
        async: true,
    });
}

pub mod wasm_bindings_http {
    wasmtime::component::bindgen!({
        path: "../carina-plugin-wit/wit",
        world: "carina-provider-with-http",
        async: true,
        with: {
            "carina:provider/types": super::wasm_bindings::carina::provider::types,
            "carina:provider/provider": super::wasm_bindings::exports::carina::provider::provider,
        },
    });
}

pub use wasm_factory::WasmProviderFactory;
