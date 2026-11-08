use super::*;
use crate::provider::{ProviderError, ProviderPlan};
use std::sync::{Arc, Mutex};

fn captured_session() -> (
    boundary::ProviderSession,
    Arc<Mutex<Vec<BoundProviderEvent>>>,
) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = ProviderPlan::state_only()
        .session("mock", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    (session, events)
}

#[test]
fn handled_failure_then_success_retains_one_warning_and_native_cause() {
    let (session, events) = captured_session();
    let error = ProviderError::api_error_caused_by(
        "retrying operation",
        std::io::Error::other("ordinary nested cause"),
    )
    .with_operation("CreateWidget")
    .with_status(429)
    .with_code("SlowDown")
    .with_request_id("request-123");
    session.diagnostics().emit(ProviderEvent::failure(
        EventLevel::Warn,
        "attempt 2 of 5".into(),
        &error,
    ));
    let success: Result<(), ProviderError> = Ok(());
    assert!(success.is_ok());
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].severity(), EventLevel::Warn);
    assert!(
        events[0]
            .failure()
            .unwrap()
            .detail()
            .cause()
            .unwrap()
            .is::<std::io::Error>()
    );
    let text = present_event(&events[0]).to_string();
    for expected in [
        "attempt 2 of 5",
        "retrying operation",
        "ordinary nested cause",
        "CreateWidget",
        "429",
        "SlowDown",
        "request-123",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
}

#[test]
fn configured_secrets_are_redacted_in_every_event_field() {
    let context = crate::test_utils::provider_text::context(&["event-input-canary"]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = context
        .session()
        .clone()
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let error = ProviderError::api_error_caused_by(
        ProviderText::new("ordinary event-input-canary"),
        std::io::Error::other("cause event-input-canary"),
    )
    .with_code("code event-input-canary")
    .with_request_id("event-input-canary");
    session.diagnostics().emit(ProviderEvent::failure(
        EventLevel::Warn,
        ProviderText::new("context event-input-canary").into(),
        &error,
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(text.contains("ordinary"));
    assert!(text.contains("(secret)"));
    assert!(!text.contains("event-input-canary"));
}

#[test]
fn guest_registration_covers_events_returned_errors_and_old_generations() {
    use carina_provider_protocol::diagnostics::ProtectedMaterial;
    let (session, events) = captured_session();
    for token in ["guest-token-first", "guest-token-refresh"] {
        session
            .diagnostics()
            .register_protected(ProtectedMaterial::GithubToken(token.into()))
            .unwrap();
    }
    let error = ProviderError::internal(ProviderText::new(
        "keep guest-token-first guest-token-refresh",
    ));
    session.diagnostics().emit(ProviderEvent::failure(
        EventLevel::Warn,
        "handled".into(),
        &error,
    ));
    let events = events.lock().unwrap();
    for text in [
        present_event(&events[0]).to_string(),
        error.with_secrets(session.redaction_set()).to_string(),
    ] {
        assert!(text.contains("keep"));
        assert!(text.contains("(secret)"));
        assert!(!text.contains("guest-token-first"));
        assert!(!text.contains("guest-token-refresh"));
    }
    let isolated = ProviderPlan::state_only().session("mock", None, None);
    assert!(
        ProviderError::internal("guest-token-first")
            .with_secrets(isolated.redaction_set())
            .to_string()
            .contains("guest-token-first")
    );
}

#[test]
fn unresolved_value_address_masks_the_whole_subtree() {
    use carina_provider_protocol::types::Value;
    let (session, events) = captured_session();
    let value = Value::Map(std::collections::BTreeMap::from([(
        "unknown-key-canary".into(),
        Value::String("unknown-leaf-canary".into()),
    )]));
    session.diagnostics().emit(ProviderEvent::value(
        EventLevel::Warn,
        ProviderValueDiagnostic::typed(None, &[], ValueOrigin::Observed, &value),
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(text.contains("(secret)"));
    assert!(!text.contains("unknown-key-canary"));
    assert!(!text.contains("unknown-leaf-canary"));
}

#[test]
fn event_count_is_bounded() {
    let (session, events) = captured_session();
    let diagnostic = ProviderDiagnostic::message(ProviderText::new("ordinary warning"));
    for _ in 0..10_000 {
        session.diagnostics().emit(ProviderEvent::diagnostic(
            EventLevel::Warn,
            "bounded".into(),
            &diagnostic,
        ));
    }
    let events = events.lock().unwrap();
    assert!(!events.is_empty());
    assert!(events.len() <= 256);
}

#[test]
fn redaction_precedes_output_truncation() {
    let context = crate::test_utils::provider_text::context(&["secret-at-cutoff-canary"]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = context
        .session()
        .clone()
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let text = format!(
        "{}secret-at-cutoff-canary{}",
        "x".repeat(MAX_PROVIDER_TEXT_BYTES - 30),
        "y".repeat(200)
    );
    let diagnostic = ProviderDiagnostic::message(ProviderText::new(text));
    session.diagnostics().emit(ProviderEvent::diagnostic(
        EventLevel::Warn,
        "cutoff".into(),
        &diagnostic,
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(text.len() <= MAX_PROVIDER_TEXT_BYTES);
    assert!(!text.contains("secret-at"));
    assert!(text.contains("truncated"));
}

fn value_session(
    secret_input: bool,
) -> (
    boundary::ProviderSession,
    Arc<Mutex<Vec<BoundProviderEvent>>>,
    carina_provider_protocol::types::ResourceId,
) {
    use crate::resource::{ConcreteValue, DeferredValue, Resource, Value};
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
    };
    let mut resource: Resource = Resource::with_provider("mock", "Widget", "host-address", None);
    if secret_input {
        resource.attributes.insert(
            "nested".into(),
            Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
                "password".into(),
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String(
                        "original-secret".into(),
                    )))
                    .into(),
                )),
            )]))),
        );
    }
    let plan = ProviderPlan::from_desired(&[resource], &[]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = plan
        .session("mock", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let schema = ResourceSchema::new("Widget")
        .attribute(AttributeSchema::optional("public", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("write_only", AttributeType::string())
                .write_only()
                .build(),
        )
        .attribute(
            AttributeSchema::optional(
                "nested",
                AttributeType::struct_(
                    "Nested",
                    vec![
                        StructField::optional("password", AttributeType::string()).build(),
                        StructField::optional("visible", AttributeType::string()).build(),
                        StructField::optional("write_only", AttributeType::string())
                            .write_only()
                            .build(),
                    ],
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema);
    session.diagnostics().set_schemas(schemas);
    (
        session,
        events,
        carina_provider_protocol::types::ResourceId {
            provider: "mock".into(),
            resource_type: "Widget".into(),
            identity: "host-address".into(),
        },
    )
}

#[test]
fn observed_origin_never_downgrades_host_secret_classification() {
    let (session, events, id) = value_session(true);
    let candidate = serde_json::json!({"password": "new-observed-secret-canary", "visible": "ordinary-sibling"});
    session.diagnostics().emit(ProviderEvent::value(
        EventLevel::Warn,
        ProviderValueDiagnostic::json(
            Some(&id),
            &["nested".into()],
            ValueOrigin::Observed,
            &candidate,
        ),
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(!text.contains("new-observed-secret-canary"));
    assert!(text.contains("ordinary-sibling"));
}

#[test]
fn write_only_and_unclassified_values_are_masked() {
    let (session, events, id) = value_session(false);
    let candidate = carina_provider_protocol::types::Value::String("write-only-canary".into());
    for (path, origin) in [
        ("write_only", ValueOrigin::Observed),
        ("public", ValueOrigin::Unclassified),
    ] {
        session.diagnostics().emit(ProviderEvent::value(
            EventLevel::Warn,
            ProviderValueDiagnostic::typed(Some(&id), &[path.into()], origin, &candidate),
        ));
    }
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    for event in &*events {
        assert!(
            !present_event(event)
                .to_string()
                .contains("write-only-canary")
        );
    }
}

#[test]
fn public_value_leaves_and_keys_use_the_session_redactor() {
    use carina_provider_protocol::diagnostics::ProtectedMaterial;
    let (session, events, id) = value_session(false);
    session
        .diagnostics()
        .register_protected(ProtectedMaterial::GithubToken("leaf-canary".into()))
        .unwrap();
    let candidate = carina_provider_protocol::types::Value::String("ordinary leaf-canary".into());
    session.diagnostics().emit(ProviderEvent::value(
        EventLevel::Warn,
        ProviderValueDiagnostic::typed(
            Some(&id),
            &["public".into()],
            ValueOrigin::Observed,
            &candidate,
        ),
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(text.contains("ordinary"));
    assert!(!text.contains("leaf-canary"));
}

#[test]
fn oversized_and_deep_masked_values_are_rejected_before_binding() {
    let (session, events) = captured_session();
    let huge = carina_provider_protocol::types::Value::String("x".repeat(100_000));
    session.diagnostics().emit(ProviderEvent::value(
        EventLevel::Warn,
        ProviderValueDiagnostic::typed(None, &[], ValueOrigin::Unclassified, &huge),
    ));
    let mut deep = carina_provider_protocol::types::Value::String("deep-canary".into());
    for _ in 0..40 {
        deep = carina_provider_protocol::types::Value::List(vec![deep]);
    }
    session.diagnostics().emit(ProviderEvent::value(
        EventLevel::Warn,
        ProviderValueDiagnostic::typed(None, &[], ValueOrigin::Unclassified, &deep),
    ));
    assert!(events.lock().unwrap().is_empty());
}

#[test]
fn registration_limit_failure_is_atomic_and_does_not_echo_input() {
    use carina_provider_protocol::diagnostics::{AwsSession, ProtectedMaterial, RegistrationError};
    let (session, _events) = captured_session();
    let error = session
        .diagnostics()
        .register_protected(ProtectedMaterial::AwsSession(AwsSession {
            access_key_id: "atomic-registration-canary".into(),
            secret_access_key: "x".repeat(300_000),
            session_token: None,
        }))
        .unwrap_err();
    assert_eq!(error, RegistrationError::LimitExceeded);
    assert!(!error.to_string().contains("atomic-registration-canary"));
    let text = ProviderError::internal("atomic-registration-canary")
        .with_secrets(session.redaction_set())
        .to_string();
    assert!(text.contains("atomic-registration-canary"));
}

#[test]
fn unresolved_nested_member_masks_its_key_and_value() {
    let (session, events, id) = value_session(false);
    let candidate = serde_json::json!({"unknown-key-secret-canary": "unknown-value-secret-canary", "visible": "ordinary-sibling"});
    session.diagnostics().emit(ProviderEvent::value(
        EventLevel::Warn,
        ProviderValueDiagnostic::json(
            Some(&id),
            &["nested".into()],
            ValueOrigin::Observed,
            &candidate,
        ),
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(!text.contains("unknown-key-secret-canary"));
    assert!(!text.contains("unknown-value-secret-canary"));
    assert!(text.contains("ordinary-sibling"));
}

#[test]
fn a_runtime_address_without_checked_input_coverage_cannot_authorize_value_text() {
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let (session, events) = captured_session();
    let id =
        crate::resource::ResourceId::with_provider_identity("mock", "Widget", "runtime-only", None);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Widget")
            .attribute(AttributeSchema::optional("password", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    session.diagnostics().set_schemas(schemas);
    session.diagnostics().include_address(&id);
    let wire_id = carina_provider_protocol::types::ResourceId {
        provider: "mock".into(),
        resource_type: "Widget".into(),
        identity: "runtime-only".into(),
    };
    let candidate = carina_provider_protocol::types::Value::String("runtime-secret-canary".into());
    session.diagnostics().emit(ProviderEvent::value(
        EventLevel::Warn,
        ProviderValueDiagnostic::typed(
            Some(&wire_id),
            &["password".into()],
            ValueOrigin::Observed,
            &candidate,
        ),
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(!text.contains("runtime-secret-canary"));
    assert!(text.contains("(secret)"));
}

#[test]
fn presenter_redacts_a_secret_assembled_across_bound_segments() {
    let context = crate::test_utils::provider_text::context(&["cross-boundary: secret-value"]);
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = context
        .session()
        .clone()
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
    let diagnostic = ProviderDiagnostic::message("secret-value".into());
    session.diagnostics().emit(ProviderEvent::diagnostic(
        EventLevel::Warn,
        "cross-boundary".into(),
        &diagnostic,
    ));
    let events = events.lock().unwrap();
    let text = present_event(&events[0]).to_string();
    assert!(!text.contains("cross-boundary: secret-value"));
    assert!(text.contains("(secret)"));
}
