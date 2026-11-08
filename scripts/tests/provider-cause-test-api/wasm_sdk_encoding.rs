use carina_plugin_sdk::types::{PartialReadDiagnostic, ProviderError, UpdateOutcome};

pub fn encode_error(error: ProviderError) {
    let _encoded = carina_plugin_sdk::wasm_guest::encode_provider_error(error);
}

pub fn encode_partial(partial: &PartialReadDiagnostic) {
    let _encoded = carina_plugin_sdk::wasm_guest::encode_partial(partial);
}

pub fn encode_create(step: carina_plugin_sdk::ProtocolCreateStep<()>) {
    let _encoded = carina_plugin_sdk::wasm_guest::encode_create_step(step);
}

pub fn encode_resume(step: carina_plugin_sdk::ProtocolResumeStep<()>) {
    let _encoded = carina_plugin_sdk::wasm_guest::encode_resume_step(step);
}

pub fn encode_update(outcome: &UpdateOutcome) {
    let _encoded = carina_plugin_sdk::wasm_guest::encode_update_result(outcome);
}
