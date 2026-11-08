use carina_plugin_sdk::protocol::types as wire;
use carina_plugin_sdk::types::ProviderError;

pub fn decode_error(error: wire::ProviderError) -> ProviderError {
    ProviderError::from_wire(error)
}

pub fn decode_reference(error: wire::ProviderError) {
    let _ = carina_plugin_sdk::core::references::ReferenceOutcome::from_guest(
        wire::ReferenceOutcome::NotFound(Box::new(error)),
    );
}
