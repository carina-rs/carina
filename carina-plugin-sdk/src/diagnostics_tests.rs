use crate::{EventLevel, ProviderDiagnostics, ProviderEvent};
use carina_core::provider::{ProviderError, ProviderPlan};
use carina_core::provider_text::present_event;
use carina_core::test_utils::provider_text::captured_session;

#[test]
fn ordinary_query_values_do_not_mask_later_diagnostics() {
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("native", None, None));
    let diagnostics = ProviderDiagnostics::from_session(&session);
    diagnostics.prepare_request(
        http::Request::builder()
            .uri("https://example.com/?list-type=2")
            .body(Vec::new())
            .unwrap(),
    );
    let error = ProviderError::internal("after 2 retries");
    diagnostics.emit(ProviderEvent::failure(
        EventLevel::Warn,
        "retry".into(),
        &error,
    ));
    assert!(
        present_event(&events.lock().unwrap()[0])
            .to_string()
            .contains("after 2 retries")
    );
}

#[test]
fn repeated_credential_adoption_is_idempotent() {
    let session = ProviderPlan::state_only().session("native", None, None);
    let diagnostics = ProviderDiagnostics::from_session(&session);
    for _ in 0..2000 {
        diagnostics
            .register_github_token(crate::protected_material::GithubTokenMaterial::new(
                "same-generation".into(),
            ))
            .unwrap();
    }
    assert!(
        !session
            .guest_inventory()
            .would_exceed_secret_limit(&session.guest_inputs(), 1)
    );
}

#[test]
fn native_sdk_handle_emits_borrowed_errors_before_success() {
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("native", None, None));
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
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("native", None, None));
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
fn native_registration_and_emission_stop_when_the_host_context_closes() {
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("native", None, None));
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

#[cfg(feature = "aws")]
#[tokio::test]
async fn apply_replaces_credentials_and_preserves_remaining_settings() {
    use crate::protected_material::AwsSessionMaterial;
    use crate::wasi_http::AwsSdkParts;
    use aws_credential_types::{
        Credentials,
        provider::{ProvideCredentials, SharedCredentialsProvider},
    };
    use aws_types::{SdkConfig, region::Region};

    let session = ProviderPlan::state_only().session("aws", None, None);
    let source = super::aws_session_tests::StaticSource(AwsSessionMaterial::new(
        aws_credential_types::Credentials::new(
            "registered-access",
            "registered-secret",
            Some("registered-token".into()),
            None,
            "pairing-test-source",
        ),
    ));
    let builder = SdkConfig::builder()
        .region(Region::new("us-west-2"))
        .credentials_provider(SharedCredentialsProvider::new(Credentials::new(
            "unregistered-access",
            "unregistered-secret",
            None,
            None,
            "previous-builder-provider",
        )));
    let config = AwsSdkParts::new(source, ProviderDiagnostics::from_session(&session))
        .apply(builder.build().into_builder())
        .build();
    let credentials = config
        .credentials_provider()
        .unwrap()
        .provide_credentials()
        .await
        .unwrap();
    assert_eq!(credentials.access_key_id(), "registered-access");
    assert_eq!(credentials.secret_access_key(), "registered-secret");
    assert_eq!(credentials.session_token(), Some("registered-token"));
    assert_eq!(config.region().unwrap().as_ref(), "us-west-2");
    assert!(config.http_client().is_some());
}
