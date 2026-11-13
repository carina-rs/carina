use carina_core::provider::{ProviderPlan, ProviderReadyConfig, ProviderValidationInputs};
use carina_core::provider_text::GuestInputInventory;
use carina_core::schema::ValidatedProviderSchemaWitness;

pub fn fresh_session(plan: &ProviderPlan, config: &ProviderReadyConfig) {
    let _ = plan.session(config);
}
pub fn fresh_builder(plan: &ProviderPlan, config: &ProviderReadyConfig) {
    let _ = plan.session_builder(config);
}
pub fn inherit(inputs: &ProviderValidationInputs) {
    let _ = inputs.inherit_diagnostic_host(inputs);
}
pub fn replace(inputs: &ProviderValidationInputs) {
    let _ = inputs.for_new_guest();
}
pub fn budget(inputs: &GuestInputInventory) {
    let _ = inputs.for_diagnostic_call();
}
pub fn schemas(inputs: &ProviderValidationInputs, witness: &ValidatedProviderSchemaWitness) {
    let _ = inputs.install_event_schemas(witness);
}

pub fn host_trace_setting() {
    let _ = carina_core::provider_text::provider_http_trace_enabled();
}
