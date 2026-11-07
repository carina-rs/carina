use carina_core::provider::{ProviderError, ProviderErrorKind};
use carina_core::test_utils::provider_text::{BindProviderText, NativeWireFixture, context};

#[derive(Debug)]
enum IamPolicyDecodeError {
    UrlDecode(std::string::FromUtf8Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for IamPolicyDecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::UrlDecode(_) => "URL decode failed",
            Self::Json(_) => "JSON parse failed",
        })
    }
}

impl std::error::Error for IamPolicyDecodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UrlDecode(error) => Some(error),
            Self::Json(error) => Some(error),
        }
    }
}

#[test]
fn role_read_policy_json_failure_keeps_its_typed_cause() {
    for raw in [
        "",
        "{",
        r#"{"Statement":[{"Condition":{"NumericLessThan":{"example:Limit":1e999}}}]}"#,
        r#"{"Statement":[{"Condition":{"NumericLessThan":{"example:Limit":-1e999}}}]}"#,
    ] {
        let error = ProviderError::api_error_caused_by(
            "Failed to decode assume role policy document",
            IamPolicyDecodeError::Json(serde_json::from_str::<serde_json::Value>(raw).unwrap_err()),
        );
        assert_eq!(error.kind(), ProviderErrorKind::ApiError);
        let decode = error
            .detail()
            .cause()
            .unwrap()
            .downcast_ref::<IamPolicyDecodeError>()
            .expect("typed IAM policy decode error");
        assert!(decode.matches_for_test(|e: &IamPolicyDecodeError| {
            matches!(e, IamPolicyDecodeError::Json(_))
        }));
        assert!(!decode.matches_for_test(|e: &IamPolicyDecodeError| {
            matches!(e, IamPolicyDecodeError::UrlDecode(_))
        }));
        assert!(decode.source().unwrap().is::<serde_json::Error>());

        let host = error.into_host_for_test();
        let decode = host
            .detail()
            .cause()
            .unwrap()
            .downcast_ref::<IamPolicyDecodeError>()
            .expect("typed IAM policy decode error after ingestion");
        assert!(decode.matches_for_test(|e: &IamPolicyDecodeError| {
            matches!(e, IamPolicyDecodeError::Json(_))
        }));
        assert!(decode.source().unwrap().is::<serde_json::Error>());
    }
}

#[test]
fn role_read_policy_url_decode_failure_keeps_its_typed_cause() {
    let error = ProviderError::api_error_caused_by(
        "Failed to decode assume role policy document",
        IamPolicyDecodeError::UrlDecode(String::from_utf8(vec![0xff]).unwrap_err()),
    );
    let decode = error
        .detail()
        .cause()
        .unwrap()
        .downcast_ref::<IamPolicyDecodeError>()
        .expect("typed IAM policy decode error");
    assert!(decode.matches_for_test(|e: &IamPolicyDecodeError| {
        matches!(e, IamPolicyDecodeError::UrlDecode(_))
    }));
    assert!(!decode.matches_for_test(|e: &IamPolicyDecodeError| {
        matches!(e, IamPolicyDecodeError::Json(_))
    }));
    assert!(decode.source().unwrap().is::<std::string::FromUtf8Error>());
}

#[test]
fn cause_variant_predicate_does_not_run_for_an_unrelated_type() {
    let error = ProviderError::api_error_caused_by(
        "Failed to decode assume role policy document",
        IamPolicyDecodeError::Json(serde_json::from_str::<serde_json::Value>("{").unwrap_err()),
    );
    let cause = error.detail().cause().unwrap().as_cause();
    assert!(!cause.matches_for_test::<std::io::Error>(|_| {
        panic!("a predicate for an unrelated type must not run")
    }));
}

#[test]
fn read_error_retains_typed_source_through_final_completion() {
    let inner_provider_error =
        ProviderError::api_error_caused_by("read denied", std::io::Error::other("source witness"));
    let reason = ProviderError::api_error_caused_by("read error", inner_provider_error);
    let source = reason
        .detail()
        .cause()
        .unwrap()
        .as_cause()
        .source()
        .unwrap();
    assert!(
        source.is::<std::io::Error>(),
        "typed I/O source was discarded"
    );
    assert_eq!(format!("{source:?}"), "<provider cause>");

    let reason = reason.into_host_for_test();
    let source = reason
        .detail()
        .cause()
        .unwrap()
        .as_cause()
        .source()
        .unwrap();
    assert!(
        source.is::<std::io::Error>(),
        "typed I/O source was discarded"
    );
    assert_eq!(format!("{source:?}"), "<provider cause>");
    assert_eq!(reason.detail().context(), "read error");
    assert!(reason.to_string().contains("source witness"));
}

#[test]
fn native_wire_fixture_preserves_provider_error_fields() {
    let error = ProviderError::internal_caused_by(
        "invalid duration",
        std::io::Error::other("duration in seconds"),
    );
    let wire = error.to_wire();
    assert_eq!(
        wire.kind,
        carina_core::provider::WireProviderErrorKind::Internal
    );
    assert_eq!(wire.message, "invalid duration");
    assert_eq!(wire.cause.as_deref(), Some("duration in seconds"));
    assert!(!format!("{error:?}").contains("duration in seconds"));
}

#[test]
fn native_binding_fixture_preserves_nested_typed_cause_after_ingestion() {
    let error = ProviderError::api_error_caused_by(
        "read error",
        ProviderError::api_error_caused_by(
            "inner context",
            std::io::Error::other("ResourceNotFoundException"),
        ),
    );
    let error = error.into_host_for_test();
    assert_eq!(error.kind(), ProviderErrorKind::ApiError);
    assert!(error.to_string().contains("ResourceNotFoundException"));
    assert_eq!(error.detail().context(), "read error");
    let diagnostic = error.detail().cause().unwrap();
    assert!(diagnostic.source_is::<std::io::Error>());
    let source = diagnostic.as_cause().source().unwrap();
    assert!(source.is::<std::io::Error>());
    assert_eq!(format!("{source:?}"), "<provider cause>");
    assert!(std::error::Error::source(&error).is_none());
    assert!(std::error::Error::source(diagnostic).is_none());
}

#[test]
fn native_binding_fixture_keeps_carried_secrets_across_nested_errors() {
    let inner: ProviderError = ProviderError::api_error_caused_by(
        "inner context",
        std::io::Error::other("inner-password outer-password"),
    )
    .with_secrets_for_test(&context(&["inner-password"]))
    .into();
    let outer = ProviderError::api_error_caused_by("read error", inner);
    let wire = outer.to_wire();
    let unbound = format!("{outer:?} {wire:?}");
    assert!(!unbound.contains("inner-password"));
    assert!(wire.cause.as_deref().unwrap().contains("(secret)"));

    let host = outer.with_secrets_for_test(&context(&["outer-password"]));
    let diagnostic = host.detail().cause().unwrap();
    let source = diagnostic.source().unwrap();
    assert!(source.is::<std::io::Error>());
    let rendered = format!(
        "{host} {host:?} {diagnostic} {diagnostic:?} {source:?} {}",
        serde_json::to_string(&host.to_wire()).unwrap(),
    );
    assert!(rendered.contains("inner context"));
    assert!(!rendered.contains("inner-password"));
    assert!(!rendered.contains("outer-password"));
}

#[test]
fn native_binding_fixture_binds_partial_read_diagnostics() {
    let diagnostic = carina_core::provider::PartialReadDiagnostic::new(
        "read password".into(),
        vec!["name".into()],
    )
    .unwrap()
    .with_secrets_for_test(&context(&["password"]));
    assert_eq!(diagnostic.reason(), "read (secret)");
    assert_eq!(diagnostic.missing_attributes()[0].as_str(), "name");
}
