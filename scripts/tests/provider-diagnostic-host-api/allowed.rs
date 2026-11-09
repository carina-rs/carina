use carina_core::provider::ProviderSession;
use carina_core::provider_text::{EventLevel, ProviderDiagnostic, ProviderEvent};

pub fn emit(session: &ProviderSession, diagnostic: &ProviderDiagnostic) {
    session.diagnostics().emit(ProviderEvent::diagnostic(
        EventLevel::Warn,
        "provider operation".into(),
        diagnostic,
    ));
}
