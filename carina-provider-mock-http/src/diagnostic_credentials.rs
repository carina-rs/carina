//! Guest-derived credential diagnostics, using the paired AWS setup.
use carina_plugin_sdk::core::provider_text::{ProviderDiagnostic, ProviderText};
use carina_plugin_sdk::protected_material::{
    AwsSessionFuture, AwsSessionMaterial, AwsSessionSource, GithubTokenMaterial,
};
use carina_plugin_sdk::types::{ProviderError, ProviderErrorKind, ReadOutcome, ResourceId};
use carina_plugin_sdk::{EventLevel, ProviderDiagnostics, ProviderEvent};

pub(crate) fn read(id: &ResourceId, mode: &str) -> Result<Option<ReadOutcome>, ProviderError> {
    let register = match mode {
        "__mock_diag_registered__" | "__mock_diag_register_only__" => true,
        "__mock_diag_emit_registered_only__" => false,
        _ => return Ok(None),
    };
    let diagnostics = ProviderDiagnostics::guest();
    if register {
        struct DiagnosticSource;
        impl AwsSessionSource for DiagnosticSource {
            fn current_session(&self) -> AwsSessionFuture<'_> {
                Box::pin(async {
                    Ok(AwsSessionMaterial::new(
                        "guest-access-canary".into(),
                        "guest-key-canary".into(),
                        Some("guest-token-canary".into()),
                        None,
                    ))
                })
            }
        }
        let config =
            carina_plugin_sdk::wasi_http::AwsSdkParts::new(DiagnosticSource, diagnostics.clone())
                .apply(Default::default())
                .build();
        let credentials = config
            .credentials_provider()
            .expect("paired AWS setup installs a credentials provider");
        futures::executor::block_on(credentials.as_ref().provide_credentials())
            .map_err(|error| ProviderError::from_diagnostic(ProviderErrorKind::Internal, error))?;
        diagnostics
            .register_github_token(GithubTokenMaterial::new("guest-github-canary".into()))
            .map_err(|error| ProviderError::from_diagnostic(ProviderErrorKind::Internal, error))?;
    }
    if mode != "__mock_diag_register_only__" {
        let diagnostic = ProviderDiagnostic::message(ProviderText::new(
            "ordinary registered context guest-access-canary guest-key-canary guest-token-canary guest-github-canary",
        ));
        diagnostics.emit(ProviderEvent::diagnostic(
            EventLevel::Warn,
            "registered".into(),
            &diagnostic,
        ));
    }
    Ok(Some(ReadOutcome::NotFound { id: id.clone() }))
}
