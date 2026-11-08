use carina_plugin_sdk::protocol::types as wire;
use carina_plugin_sdk::types::{PartialReadDiagnostic, ProviderError, UpdateOutcome};

pub fn encode_error(error: ProviderError) -> wire::ProviderError {
    carina_plugin_sdk::wasm_guest::encode_provider_error(error)
}

pub fn encode_partial(partial: &PartialReadDiagnostic) -> wire::PartialReadDiagnostic {
    carina_plugin_sdk::wasm_guest::encode_partial(partial)
}

pub fn encode_create(step: carina_plugin_sdk::ProtocolCreateStep<()>) {
    let _encoded = carina_plugin_sdk::wasm_guest::encode_create_step(step);
}

pub fn encode_resume(step: carina_plugin_sdk::ProtocolResumeStep<()>) {
    let _encoded = carina_plugin_sdk::wasm_guest::encode_resume_step(step);
}

pub fn encode_update(outcome: &UpdateOutcome) -> wire::UpdateOutcome {
    carina_plugin_sdk::wasm_guest::encode_update_result(outcome)
}

pub fn error_message(error: ProviderError) -> String {
    error.to_wire().message
}

pub fn partial_reason(partial: PartialReadDiagnostic) -> String {
    partial.to_wire().reason
}

pub fn serialize_error(error: ProviderError) -> String {
    serde_json::to_string(&encode_error(error)).unwrap()
}

pub fn serialize_partial(partial: &PartialReadDiagnostic) -> String {
    serde_json::to_string(&encode_partial(partial)).unwrap()
}
