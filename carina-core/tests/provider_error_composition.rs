//! Provider-shaped examples of every supported composition in #4325.
use carina_core::provider::{
    NonEmptyMissingAttributes, PartialReadDiagnostic, ProviderError, ProviderErrorKind,
    WireProviderError, WireProviderErrorKind,
};
use carina_core::provider_text::{
    ClassifiedSource, Diagnostic, ProviderCause, ProviderDiagnostic, ProviderText, Retryability,
    TextState,
};
use carina_core::test_utils::provider_text::{BindProviderText, NativeWireFixture, context};
use std::io::{Error, ErrorKind};

#[derive(Debug, thiserror::Error)]
#[error("service rejected request")] // error-chain: caller-renders-sources
struct SdkFailure(#[source] Error);

fn leaf() -> Error {
    Error::new(ErrorKind::PermissionDenied, "context-secret cause-secret")
}

fn reaches_leaf(mut cause: ProviderCause<'_>) -> usize {
    let mut links = 0;
    loop {
        if cause.is::<Error>() {
            assert!(cause.matches_for_test(|e: &Error| e.kind() == ErrorKind::PermissionDenied));
            return links;
        }
        cause = cause
            .source()
            .expect("every opaque frame must have a logical source");
        links += 1;
        assert!(links < 20, "acyclic chain");
    }
}

fn assert_cause<S: TextState>(diagnostic: &Diagnostic<S>) -> usize {
    assert!(
        diagnostic
            .as_cause()
            .matches_for_test(|e: &Error| { e.kind() == ErrorKind::PermissionDenied })
    );
    reaches_leaf(diagnostic.as_cause())
}

fn check(error: ProviderError, kind: ProviderErrorKind, retry: Retryability, typed: bool) {
    assert_eq!(error.kind(), kind);
    assert_eq!(error.retryability(), retry);
    assert_eq!(error.is_transient(), retry == Retryability::Transient);
    assert!(!format!("{error:?}").contains("cause-secret"));
    let depth = typed.then(|| assert_cause(error.detail().cause().unwrap()));
    let host = error.with_secrets_for_test(&context(&["context-secret", "cause-secret"]));
    assert_eq!(host.kind(), kind);
    assert_eq!(host.retryability(), retry);
    assert_eq!(
        typed.then(|| assert_cause(host.detail().cause().unwrap())),
        depth
    );
    let rendered = format!("{host} {host:?} {:?}", host.to_wire());
    assert!(!rendered.contains("context-secret"), "{rendered}");
    assert!(!rendered.contains("cause-secret"), "{rendered}");
}

#[test]
fn static_message_without_cause() {
    check(
        ProviderError::invalid_input("invalid configuration"),
        ProviderErrorKind::InvalidInput,
        Retryability::Unclassified,
        false,
    );
}

#[test]
fn runtime_message_without_cause() {
    check(
        ProviderError::api_error(ProviderText::new(format!("read {}", "context-secret"))),
        ProviderErrorKind::ApiError,
        Retryability::Unclassified,
        false,
    );
}

#[test]
fn static_context_foreign_source() {
    check(
        ProviderError::not_found_caused_by("read failed", leaf()),
        ProviderErrorKind::NotFound,
        Retryability::Unclassified,
        true,
    );
}

#[test]
fn static_context_sdk_source_with_policy_and_metadata() {
    let error = ProviderError::api_error_caused_by(
        "service read",
        ClassifiedSource::new(SdkFailure(leaf()), Retryability::Transient),
    )
    .with_operation("service.Get context-secret")
    .with_status(503)
    .with_code("Unavailable")
    .with_request_id("cause-secret");
    assert!(error.detail().cause().unwrap().source_is::<Error>());
    check(
        error,
        ProviderErrorKind::ApiError,
        Retryability::Transient,
        true,
    );
}

#[test]
fn static_context_sdk_source_without_policy() {
    check(
        ProviderError::api_error_caused_by("service read", SdkFailure(leaf())).with_status(503),
        ProviderErrorKind::ApiError,
        Retryability::Unclassified,
        true,
    );
}

#[test]
fn runtime_context_foreign_source_with_metadata() {
    check(
        ProviderError::timeout_caused_by(ProviderText::new("resource context-secret"), leaf())
            .with_operation("service.Read"),
        ProviderErrorKind::Timeout,
        Retryability::Unclassified,
        true,
    );
}

#[test]
fn foreign_source_without_added_context() {
    let error = ProviderError::from_diagnostic(ProviderErrorKind::Internal, leaf());
    let wire = NativeWireFixture::to_wire(&error);
    assert_eq!(wire.message, "context-secret cause-secret");
    assert_eq!(wire.cause, None, "captured diagnostics render once");
    check(
        error,
        ProviderErrorKind::Internal,
        Retryability::Unclassified,
        true,
    );
}

fn read_error() -> ProviderError {
    ProviderError::api_error_caused_by("read denied", leaf())
        .with_retryability(Retryability::Transient)
}

#[test]
fn static_context_provider_error() {
    check(
        ProviderError::internal_caused_by("read-back failed", read_error()),
        ProviderErrorKind::Internal,
        Retryability::Transient,
        true,
    );
}

#[test]
fn runtime_context_provider_error() {
    let context = ProviderText::new("missing context-secret")
        .with_context("post-update read-back incomplete");
    check(
        ProviderError::api_error_caused_by(context, read_error()),
        ProviderErrorKind::ApiError,
        Retryability::Transient,
        true,
    );
}

#[test]
fn provider_error_as_partial_reason() {
    let diagnostic = PartialReadDiagnostic::new(ProviderDiagnostic::new(read_error()), missing());
    assert_eq!(
        diagnostic.diagnostic().retryability(),
        Retryability::Transient
    );
    assert_cause(diagnostic.diagnostic());
    let host = diagnostic.with_secrets_for_test(&context(&["context-secret", "cause-secret"]));
    assert_eq!(host.diagnostic().retryability(), Retryability::Transient);
    assert_cause(host.diagnostic());
    assert_eq!(host.reason(), "read denied: (secret) (secret)");
    let json = serde_json::to_value(&host).unwrap();
    assert_eq!(json["reason"], "read denied: (secret) (secret)");
    let decoded: PartialReadDiagnostic = serde_json::from_value(json).unwrap();
    assert_eq!(
        decoded.diagnostic().retryability(),
        Retryability::Unclassified
    );
    assert!(
        !decoded
            .diagnostic()
            .as_cause()
            .matches_for_test::<Error>(|_| true)
    );
    assert_eq!(decoded.into_host_for_test(), host);
}

fn missing() -> NonEmptyMissingAttributes {
    vec!["name".to_owned()].try_into().unwrap()
}

#[test]
fn runtime_message_as_partial_reason() {
    let diagnostic = PartialReadDiagnostic::new(
        ProviderDiagnostic::message(ProviderText::new("missing context-secret")),
        missing(),
    );
    assert_eq!(
        diagnostic.diagnostic().retryability(),
        Retryability::Unclassified
    );
    assert_eq!(
        diagnostic
            .with_secrets_for_test(&context(&["context-secret"]))
            .reason(),
        "missing (secret)"
    );
    assert!(NonEmptyMissingAttributes::try_from(Vec::<String>::new()).is_err());
}

#[test]
fn flattened_wire_source() {
    let error = ProviderError::from_wire(WireProviderError {
        kind: WireProviderErrorKind::ApiError,
        message: "read context-secret".into(),
        cause: Some("cause-secret".into()),
        resource_id: None,
        provider_name: Some("service".into()),
        operation: Some("Get".into()),
        status: Some(503),
        code: None,
        request_id: None,
    });
    assert!(
        !error
            .detail()
            .cause()
            .unwrap()
            .as_cause()
            .matches_for_test::<Error>(|_| true)
    );
    check(
        error,
        ProviderErrorKind::ApiError,
        Retryability::Unclassified,
        false,
    );
}

#[test]
fn static_context_existing_diagnostic() {
    check(
        ProviderError::api_error_caused_by(
            "read-back failed",
            ProviderDiagnostic::new(read_error()),
        ),
        ProviderErrorKind::ApiError,
        Retryability::Transient,
        true,
    );
}

#[test]
fn runtime_context_existing_diagnostic() {
    check(
        ProviderError::api_error_caused_by(
            ProviderText::new("missing context-secret"),
            ProviderDiagnostic::new(read_error()),
        ),
        ProviderErrorKind::ApiError,
        Retryability::Transient,
        true,
    );
}

#[test]
fn existing_diagnostic_without_added_context() {
    let diagnostic = ProviderDiagnostic::new(read_error());
    check(
        ProviderError::from_diagnostic(ProviderErrorKind::ApiError, diagnostic),
        ProviderErrorKind::ApiError,
        Retryability::Transient,
        true,
    );
}

#[test]
fn runtime_foreign_context_needs_no_local_wrapper() {
    check(
        ProviderError::invalid_input_caused_by(
            ProviderText::new("attribute context-secret"),
            SdkFailure(leaf()),
        ),
        ProviderErrorKind::InvalidInput,
        Retryability::Unclassified,
        true,
    );
}

#[test]
fn outer_error_inherits_transient_cause() {
    let error = ProviderError::api_error_caused_by(
        "outer",
        ProviderError::internal_caused_by("middle", read_error()),
    );
    assert!(error.detail().cause().unwrap().source_is::<Error>());
    check(
        error,
        ProviderErrorKind::ApiError,
        Retryability::Transient,
        true,
    );
}

#[test]
fn explicit_retryability_overrides_inheritance() {
    for retry in [Retryability::Unclassified, Retryability::Permanent] {
        check(
            ProviderError::timeout_caused_by("outer", read_error()).with_retryability(retry),
            ProviderErrorKind::Timeout,
            retry,
            true,
        );
    }
    assert!(!ProviderError::timeout("budget exhausted").is_transient());
}

#[test]
fn shared_observation_and_verdict_have_independent_redaction_views() {
    let source: ProviderError = ProviderError::api_error_caused_by("read context-secret", leaf())
        .with_secrets_for_test(&context(&["cause-secret"]))
        .into();
    let source = ProviderDiagnostic::new(source);
    let observation: carina_core::provider::Observation =
        carina_core::provider::Observation::ReadFailed(ProviderError::internal_caused_by(
            "read-back failed",
            source.clone(),
        ));
    let verdict = ProviderError::internal_caused_by("read-back failed", source);
    let carina_core::provider::Observation::ReadFailed(observation) = observation else {
        unreachable!()
    };
    assert_cause(observation.detail().cause().unwrap());
    assert_cause(verdict.detail().cause().unwrap());
    assert_eq!(
        serde_json::to_value(NativeWireFixture::to_wire(&observation)).unwrap(),
        serde_json::to_value(NativeWireFixture::to_wire(&verdict)).unwrap()
    );
    let observation = observation.with_secrets_for_test(&context(&["context-secret"]));
    assert_cause(observation.detail().cause().unwrap());
    assert!(!observation.to_string().contains("context-secret"));
    let verdict = verdict.into_host_for_test();
    assert_cause(verdict.detail().cause().unwrap());
    assert!(
        verdict.to_string().contains("context-secret"),
        "binding a sibling must not extend shared inventories"
    );
    assert!(!verdict.to_string().contains("cause-secret"));
}

#[test]
fn traversal_enters_bound_boxed_and_shared_diagnostic_frames() {
    for boxed in [false, true] {
        let host = read_error().into_host_for_test();
        let diagnostic = ProviderDiagnostic::new(host).into_host_for_test();
        let error = if boxed {
            ProviderError::api_error_caused_by("outer", Box::new(diagnostic))
        } else {
            ProviderError::api_error_caused_by("outer", std::sync::Arc::new(diagnostic))
        };
        let cause = error.detail().cause().unwrap();
        assert!(
            cause
                .source()
                .unwrap()
                .is::<carina_core::provider::HostError>()
        );
        assert!(cause.source().unwrap().source().unwrap().is::<Error>());
        assert_eq!(reaches_leaf(cause.as_cause()), 2);
        check(
            error,
            ProviderErrorKind::ApiError,
            Retryability::Transient,
            true,
        );
    }
}
