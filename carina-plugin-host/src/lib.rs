pub mod wasm_convert;
pub mod wasm_factory;

mod desired_normalization;

#[cfg(any(test, feature = "test-utils"))]
pub use wasm_factory::GuestCallBudget;
pub use wasm_factory::{
    AwsCredentialError, AwsCredentialProvisioner, AwsCredentialSource, AwsCredentials,
    PrecompiledComponentDeserializationError, ProviderInstantiationError, WASI_HTTP_HOST_VERSION,
    WasmProviderFactory, WasmProviderLoadError,
};
