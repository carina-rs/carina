pub mod convert;
pub mod factory;
pub mod normalizer;
pub mod process;
pub mod provider;
pub mod wasm_convert;

pub mod wasm_bindings {
    wasmtime::component::bindgen!({
        path: "../carina-plugin-wit/wit",
        world: "carina-provider",
    });
}

pub use factory::ProcessProviderFactory;
pub use normalizer::ProcessProviderNormalizer;
pub use provider::ProcessProvider;
