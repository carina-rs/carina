pub mod wasm_convert;
pub mod wasm_factory;

mod desired_normalization;

#[cfg(any(test, feature = "test-utils"))]
pub use wasm_factory::GuestCallBudget;
pub use wasm_factory::{
    AwsCredentialError, AwsCredentialProvisioner, AwsCredentialSource, AwsCredentials,
    PrecompiledComponentDeserializationError, ProviderInstantiationError,
    SchemaOnlyWasmProviderFactory, WASI_HTTP_HOST_VERSION, WasmProviderFactory,
    WasmProviderLoadError,
};

#[cfg(test)]
pub(crate) fn finite(value: f64) -> carina_provider_protocol::FiniteF64 {
    carina_provider_protocol::FiniteF64::new(value).unwrap()
}
