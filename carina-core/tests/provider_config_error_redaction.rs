#[path = "support/provider_config_error.rs"]
mod support;

use carina_core::provider::{
    CustomTypeDispatchError, HostError, HostErrorCause, ProviderError, ProviderErrorKind,
    SuspendedValidationId,
};
use carina_core::provider_text::{ProviderDiagnostic, ProviderText, Retryability};
use carina_core::resource::{ConcreteValue, DeferredValue, ResourceId, Value};
use carina_core::schema::TypeError;
use carina_core::test_utils::provider_text::{BindProviderText, NativeWireFixture, context};
use indexmap::IndexMap;

const ORIGINAL: &str = "original failure ThrottlingException cause-secret-round2";
const REPLACEMENT: &str = "configuration containing a secret failed validation";

fn assert_config_redaction(error: ProviderError, expected_cause: Option<&str>) {
    let marker =
        HostErrorCause::ValidationSuspended(SuspendedValidationId::from_keyed_fingerprint([7; 32]));
    let error = error
        .with_retryability(Retryability::Transient)
        .for_resource(ResourceId::with_identity("mock", "resource"))
        .for_provider("mock")
        .with_operation("validate_config")
        .with_status(403)
        .with_code("AccessDenied")
        .with_request_id("request-id")
        .with_host_cause(marker);
    let kind = error.kind();
    let wire_before = NativeWireFixture::to_wire(&error);
    let attributes = IndexMap::from([(
        "token".into(),
        Value::Deferred(DeferredValue::Secret(
            Box::new(Value::Concrete(ConcreteValue::String(
                "cause-secret-round2".into(),
            )))
            .into(),
        )),
    )]);
    let failure = carina_core::executor::provider_ready::validate_and_canonicalize_provider_config(
        &support::NativeConfigErrorFactory(error),
        "mock",
        &attributes,
        &Default::default(),
    )
    .unwrap_err();
    let error = failure.provider_error().unwrap().clone();
    let wire = error.to_wire();
    let failure_rendered = format!("{failure} {failure:?}");
    let converted = HostError::from(failure);
    for rendered in [
        failure_rendered,
        format!("{error} {error:?}"),
        format!("{wire:?}"),
        format!("{converted} {converted:?} {:?}", converted.to_wire()),
    ] {
        assert!(!rendered.contains("cause-secret-round2"), "{rendered}");
        if let Some(cause) = expected_cause {
            assert!(rendered.contains(cause), "{rendered}");
        } else {
            assert!(!rendered.contains("original failure"), "{rendered}");
        }
        assert!(!rendered.contains("original context"), "{rendered}");
        assert!(rendered.contains(REPLACEMENT), "{rendered}");
    }
    assert_eq!(wire.message, REPLACEMENT);
    assert_eq!(wire.cause.as_deref(), expected_cause);
    assert_eq!(error.detail().cause().is_some(), expected_cause.is_some());
    if expected_cause.is_some() {
        assert!(error.detail().cause().unwrap().is::<std::io::Error>());
    }
    assert_eq!(error.kind(), kind);
    assert_eq!(error.retryability(), Retryability::Transient);
    assert!(error.is_throttled(), "policy must survive text suppression");
    assert_eq!(error.detail().host_cause(), Some(marker));
    assert_eq!(wire.kind, wire_before.kind);
    assert_eq!(wire.resource_id, wire_before.resource_id);
    assert_eq!(wire.provider_name, wire_before.provider_name);
    assert_eq!(wire.operation, wire_before.operation);
    assert_eq!(wire.status, wire_before.status);
    assert_eq!(wire.code, wire_before.code);
    assert_eq!(wire.request_id, wire_before.request_id);
}

#[test]
fn secret_configuration_totally_redacts_native_captured_failures() {
    assert_config_redaction(
        ProviderError::from_diagnostic(ProviderErrorKind::Timeout, std::io::Error::other(ORIGINAL)),
        None,
    );
}

#[test]
fn secret_configuration_preserves_native_contextual_causes_with_redaction() {
    assert_config_redaction(
        ProviderError::timeout_caused_by(
            ProviderText::new("original context"),
            std::io::Error::other(ORIGINAL),
        ),
        Some("original failure ThrottlingException (secret)"),
    );
}

#[test]
fn secret_configuration_totally_redacts_native_message_failures() {
    assert_config_redaction(ProviderError::timeout(ORIGINAL), None);
}

#[test]
fn secret_configuration_totally_redacts_host_built_captures() {
    assert_config_redaction(
        ProviderError::from_diagnostic(ProviderErrorKind::Timeout, HostError::timeout(ORIGINAL)),
        None,
    );
}

#[test]
fn secret_configuration_totally_redacts_dispatch_captures() {
    let reason = ProviderDiagnostic::new(std::io::Error::other(ORIGINAL));
    assert_config_redaction(
        CustomTypeDispatchError::Validation(Box::new(TypeError::ValidationFailed {
            message: reason.into(),
        }))
        .into_provider_error(),
        None,
    );
}

#[test]
fn secret_configuration_preserves_carried_metadata_redactions() {
    let cause =
        ProviderError::timeout_caused_by("original context", std::io::Error::other(ORIGINAL))
            .with_secrets_for_test(&context(&["request-id"]));
    assert_config_redaction(
        ProviderError::from_diagnostic(ProviderErrorKind::Timeout, cause),
        None,
    );
}
