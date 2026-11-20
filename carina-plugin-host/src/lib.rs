#![deny(clippy::iter_over_hash_type)]

#[cfg(any(test, feature = "test-utils"))]
pub mod test_utils;

pub mod wasm_factory;

pub use wasm_factory::{
    AwsCredentialError, AwsCredentialProvisioner, AwsCredentialSource, AwsCredentials,
    PrecompiledComponentDeserializationError, ProviderInstantiationError, WASI_HTTP_HOST_VERSION,
    WasmLoadNotice, WasmLoadNoticeSink, WasmProviderFactory, WasmProviderLoadError,
};
#[cfg(any(test, feature = "test-utils"))]
pub use wasm_factory::{GuestCallBudget, SchemaOnlyWasmProviderFactory};

#[cfg(test)]
pub(crate) fn finite(value: f64) -> carina_provider_protocol::FiniteF64 {
    carina_provider_protocol::FiniteF64::new(value).unwrap()
}

#[cfg(test)]
#[path = "../../carina-core/tests/support/provider_text.rs"]
mod provider_text_fixtures;
