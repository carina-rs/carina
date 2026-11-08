use carina_plugin_sdk::types::{ProviderError, ProviderErrorKind};
use carina_plugin_sdk::core::provider_text::ProviderDiagnostic;

pub fn author_error(kind: ProviderErrorKind, message: String) -> ProviderError {
    ProviderError::from_diagnostic(kind, ProviderDiagnostic::message(message.into()))
}

pub fn decode_reference(value: String) {
    let _ = carina_plugin_sdk::core::references::ReferenceOutcome::from_guest(value);
}
