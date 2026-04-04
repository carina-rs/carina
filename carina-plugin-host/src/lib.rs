pub mod wasm_convert;
pub mod wasm_factory;

pub mod wasm_bindings {
    wasmtime::component::bindgen!({
        path: "../carina-plugin-wit/wit",
        world: "carina-provider",
        require_store_data_send: true,
        exports: { default: async },
    });
}

pub mod wasm_bindings_http {
    wasmtime::component::bindgen!({
        path: "../carina-plugin-wit/wit",
        world: "carina-provider-with-http",
        require_store_data_send: true,
        exports: { default: async },
        with: {
            "carina:provider/types": super::wasm_bindings::carina::provider::types,
            "wasi:http": wasmtime_wasi_http::p2::bindings::http,
            "wasi:io": wasmtime_wasi::p2::bindings::io,
        },
    });
}

pub use wasm_factory::WasmProviderFactory;
