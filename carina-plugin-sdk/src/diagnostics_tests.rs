use crate::{EventLevel, ProviderDiagnostics, ProviderEvent};
use carina_core::provider::{ProviderError, ProviderPlan};
use carina_core::provider_text::present_event;
use std::sync::{Arc, Mutex};

#[test]
fn native_sdk_handle_emits_borrowed_errors_before_success() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = ProviderPlan::state_only()
        .session("native", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let diagnostics = ProviderDiagnostics::from_session(&session);
    let error = ProviderError::api_error_caused_by(
        "handled operation",
        std::io::Error::other("native nested cause"),
    );
    diagnostics.emit(ProviderEvent::failure(
        EventLevel::Warn,
        "retry".into(),
        &error,
    ));
    assert_eq!(
        error.kind(),
        carina_core::provider::ProviderErrorKind::ApiError
    );
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert!(
        present_event(&events[0])
            .to_string()
            .contains("native nested cause")
    );
}

#[test]
fn registration_acknowledgement_precedes_operational_adoption() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = ProviderPlan::state_only()
        .session("native", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let diagnostics = ProviderDiagnostics::from_session(&session);
    let token = diagnostics
        .register_github_token(crate::protected_material::GithubTokenMaterial::new(
            "registered-sdk-token".into(),
        ))
        .unwrap();
    assert!(!format!("{token:?}").contains("registered-sdk-token"));
    let error = ProviderError::internal("ordinary registered-sdk-token");
    diagnostics.emit(ProviderEvent::failure(
        EventLevel::Warn,
        "registered".into(),
        &error,
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(text.contains("ordinary"));
    assert!(!text.contains("registered-sdk-token"));
}

#[test]
fn failed_registration_returns_no_operational_wrapper() {
    let session = ProviderPlan::state_only().session("native", None, None);
    let diagnostics = ProviderDiagnostics::from_session(&session);
    let result = diagnostics.register_github_token(
        crate::protected_material::GithubTokenMaterial::new("x".repeat(300_000)),
    );
    assert!(matches!(
        result,
        Err(crate::protected_material::RegistrationError::LimitExceeded)
    ));
}

#[test]
fn signing_registration_covers_signature_and_decoded_query_values() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = ProviderPlan::state_only()
        .session("native", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let diagnostics = ProviderDiagnostics::from_session(&session);
    let request = http::Request::builder()
        .uri("https://example.com/?token=guest%2Fquery%2Bsecret")
        .header(
            "authorization",
            "AWS4-HMAC-SHA256 Credential=guest-access-key/date, Signature=guest-signature",
        )
        .body(Vec::new())
        .unwrap();
    let _registered = diagnostics.prepare_request(request).unwrap();
    let error =
        ProviderError::internal("ordinary guest/query+secret guest-signature guest-access-key");
    diagnostics.emit(ProviderEvent::failure(
        EventLevel::Warn,
        "signed".into(),
        &error,
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    for secret in ["guest/query+secret", "guest-signature", "guest-access-key"] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
    assert!(text.contains("ordinary"));
}

#[tokio::test]
async fn signing_registration_failure_stops_the_connector_before_wasi_handoff() {
    use crate::protected_material::{AwsSessionMaterial, GithubTokenMaterial};
    use crate::wasi_http::{SendPhase, WasiHttpClient, send_phase_of};
    use aws_smithy_runtime_api::client::http::{HttpClient, HttpConnector, HttpConnectorSettings};
    use aws_smithy_runtime_api::client::runtime_components::RuntimeComponentsBuilder;
    let session = ProviderPlan::state_only().session("native", None, None);
    let diagnostics = ProviderDiagnostics::from_session(&session);
    let registered = diagnostics
        .register_aws_session(AwsSessionMaterial::new("key".into(), "secret".into(), None))
        .unwrap();
    diagnostics
        .register_github_token(GithubTokenMaterial::new("x".repeat(250_000)))
        .unwrap();
    let components = RuntimeComponentsBuilder::for_tests().build().unwrap();
    let connector = WasiHttpClient::new(registered)
        .http_connector(&HttpConnectorSettings::builder().build(), &components);
    let mut request =
        aws_smithy_runtime_api::client::orchestrator::HttpRequest::get("https://example.com/")
            .unwrap();
    request
        .headers_mut()
        .try_insert("authorization", "y".repeat(20_000))
        .unwrap();
    let error = connector.call(request).await.unwrap_err();
    assert_eq!(send_phase_of(&error), Some(SendPhase::NotSent));
}

#[test]
fn native_registration_and_emission_stop_when_the_host_context_closes() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = ProviderPlan::state_only()
        .session("native", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let diagnostics = ProviderDiagnostics::from_session(&session);
    drop(session);
    assert!(matches!(
        diagnostics.register_github_token(crate::protected_material::GithubTokenMaterial::new(
            "closed-context-canary".into()
        )),
        Err(crate::protected_material::RegistrationError::ClosedContext)
    ));
    let error = ProviderError::internal("closed-context-canary");
    diagnostics.emit(ProviderEvent::failure(
        EventLevel::Warn,
        "closed".into(),
        &error,
    ));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn invalid_authorization_is_a_preparation_error_without_a_credential_echo() {
    let session = ProviderPlan::state_only().session("native", None, None);
    let diagnostics = ProviderDiagnostics::from_session(&session);
    let token = diagnostics
        .register_github_token(crate::protected_material::GithubTokenMaterial::new(
            "invalid\nheader-secret-canary".into(),
        ))
        .unwrap();
    let error = token.authorize(http::Request::new(Vec::new())).unwrap_err();
    assert!(error.to_string().contains("header"));
    assert!(!error.to_string().contains("header-secret-canary"));
}
