use crate::protected_material::{
    AwsSessionFuture, AwsSessionMaterial, AwsSessionSource, RegistrationError,
};
use crate::wasi_http::AwsSdkParts;
use crate::{EventLevel, ProviderDiagnostics, ProviderEvent};
use aws_credential_types::provider::ProvideCredentials;
use carina_core::provider::{ProviderError, ProviderPlan};
use carina_core::provider_text::present_event;
use carina_core::test_utils::provider_text::captured_session;
use std::error::Error;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Default)]
struct Generations(Arc<AtomicUsize>);
impl Generations {
    fn material(&self) -> AwsSessionMaterial {
        let index = self.0.fetch_add(1, Ordering::SeqCst);
        let generation = if index == 0 { "a" } else { "b" };
        AwsSessionMaterial::new(
            format!("refresh-access-{generation}"),
            format!("refresh-secret-{generation}"),
            Some(format!("refresh-token-{generation}")),
            Some(Self::expiry(if index == 0 { 0 } else { 1 })),
        )
    }
    fn expiry(index: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(2_000_000_000 + index)
    }
}
impl AwsSessionSource for Generations {
    fn current_session(&self) -> AwsSessionFuture<'_> {
        Box::pin(async { Ok(self.material()) })
    }
}
pub(super) struct StaticSource(pub AwsSessionMaterial);
impl AwsSessionSource for StaticSource {
    fn current_session(&self) -> AwsSessionFuture<'_> {
        Box::pin(async { Ok(self.0.clone()) })
    }
}
fn config(source: Generations, diagnostics: ProviderDiagnostics) -> aws_types::SdkConfig {
    AwsSdkParts::new(source, diagnostics)
        .apply(Default::default())
        .build()
}

#[tokio::test]
async fn aws_session_source_preserves_credential_expiry() {
    let session = ProviderPlan::state_only().session("aws", None, None);
    let config = config(
        Generations::default(),
        ProviderDiagnostics::from_session(&session),
    );
    let credentials = config
        .credentials_provider()
        .unwrap()
        .provide_credentials()
        .await
        .unwrap();
    assert_eq!(credentials.expiry(), Some(Generations::expiry(0)));
}

#[tokio::test]
async fn aws_session_source_refreshes_and_masks_every_generation() {
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("aws", None, None));
    let diagnostics = ProviderDiagnostics::from_session(&session);
    let source = Generations::default();
    let config = config(source.clone(), diagnostics.clone());
    let credentials = config.credentials_provider().unwrap();
    let first = credentials.provide_credentials().await.unwrap();
    let second = credentials.provide_credentials().await.unwrap();
    let error = ProviderError::internal(
        "refresh-access-a refresh-secret-a refresh-token-a refresh-access-b refresh-secret-b refresh-token-b; public=visible",
    );
    diagnostics.emit(ProviderEvent::failure(
        EventLevel::Warn,
        "after refresh".into(),
        &error,
    ));
    let text = present_event(&events.lock().unwrap()[0]).to_string();
    for value in [
        "refresh-access-a",
        "refresh-secret-a",
        "refresh-token-a",
        "refresh-access-b",
        "refresh-secret-b",
        "refresh-token-b",
    ] {
        assert!(
            !text.contains(value),
            "unregistered generation in diagnostic: {value}: {text}"
        );
    }
    assert!(text.contains("public=visible"));
    assert_eq!(text.matches("(secret)").count(), 6);
    assert_eq!(first.access_key_id(), "refresh-access-a");
    assert_eq!(second.access_key_id(), "refresh-access-b");
    assert_eq!(first.expiry(), Some(Generations::expiry(0)));
    assert_eq!(second.expiry(), Some(Generations::expiry(1)));
    assert_eq!(source.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn aws_session_source_registration_failure_preserves_its_cause() {
    let session = ProviderPlan::state_only().session("aws", None, None);
    let config = config(
        Generations::default(),
        ProviderDiagnostics::from_session(&session),
    );
    drop(session);
    let error = config
        .credentials_provider()
        .unwrap()
        .provide_credentials()
        .await
        .expect_err("credentials must not leave the source without registration acknowledgement");
    let mut cause: Option<&(dyn Error + 'static)> = Some(&error);
    let mut registered_cause = None;
    while let Some(current) = cause {
        if let Some(registration) = current.downcast_ref::<RegistrationError>() {
            registered_cause = Some(*registration);
        }
        cause = current.source();
    }
    assert_eq!(registered_cause, Some(RegistrationError::ClosedContext));
    for secret in [
        "refresh-secret-a",
        "refresh-secret-b",
        "refresh-token-a",
        "refresh-token-b",
    ] {
        assert!(!format!("{error:?}").contains(secret));
    }
}
