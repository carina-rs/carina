use carina_plugin_sdk::types::{PartialReadDiagnostic, ProviderError};
use carina_plugin_sdk::core::provider_text::ProviderDiagnostic;

pub fn error_message(error: ProviderError) -> ProviderError {
    ProviderError::api_error(error.to_wire().message)
}

pub fn partial_reason(partial: PartialReadDiagnostic) -> ProviderDiagnostic {
    ProviderDiagnostic::message(partial.to_wire().reason.into())
}

pub fn sdk_error_message(error: ProviderError) -> ProviderError {
    ProviderError::api_error(carina_plugin_sdk::wasm_guest::encode_provider_error(error).message)
}

pub fn sdk_partial_reason(partial: PartialReadDiagnostic) -> ProviderDiagnostic {
    ProviderDiagnostic::message(carina_plugin_sdk::wasm_guest::encode_partial(&partial).reason.into())
}

pub fn serialize_error(error: ProviderError) -> String {
    serde_json::to_string(&carina_plugin_sdk::wasm_guest::encode_provider_error(error)).unwrap()
}

pub fn serialize_partial(partial: PartialReadDiagnostic) -> String {
    serde_json::to_string(&carina_plugin_sdk::wasm_guest::encode_partial(&partial)).unwrap()
}

pub fn private_wire_field(error: ProviderError) -> String {
    error.to_wire().0.message
}

pub fn native_fixture(error: ProviderError) {
    let _ = carina_plugin_sdk::core::test_utils::provider_text::NativeWireFixture::to_wire(&error);
}

pub fn native_round_trip_fixture(error: ProviderError) {
    let _ = carina_plugin_sdk::core::test_utils::provider_text::NativeWireFixture::round_trip(&error);
}
