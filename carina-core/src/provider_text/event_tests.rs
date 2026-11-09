use super::*;
use crate::provider::{ProviderError, ProviderPlan};
use crate::test_utils::provider_text::captured_session;
use std::sync::{Arc, Mutex};

#[test]
fn inherited_detached_notice_retains_the_host_sink() {
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("host", None, None));
    let value: crate::resource::Value =
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::String("ordinary".into()));
    let inputs = boundary::ProviderValidationInputs::collect("validation", [&value])
        .inherit_diagnostic_host(&session.guest_inputs());
    let channel = inputs.diagnostics();
    std::thread::spawn(move || {
        let diagnostic = ProviderDiagnostic::message("inherited detached warning".into());
        for _ in 0..512 {
            channel.emit(ProviderEvent::diagnostic(
                EventLevel::Warn,
                "detached".into(),
                &diagnostic,
            ));
        }
    })
    .join()
    .unwrap();
    let admitted = events.lock().unwrap().len();
    assert!((1..=256).contains(&admitted));
    drop(session);
    assert_eq!(events.lock().unwrap().len(), admitted);
    drop(inputs); // Its channel field drops before its last diagnostic owner.
    let captured = events.lock().unwrap();
    assert_eq!(captured.len(), admitted + 1);
    assert_eq!(
        present_event(captured.last().unwrap()).to_string(),
        format!("{} provider events suppressed", 512 - admitted)
    );
}

#[test]
fn default_admission_filters_before_separate_level_budgets() {
    const CHILD: &str = "CARINA_4334_FIX3_DEFAULT_BUDGET_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let session = ProviderPlan::state_only().session("trace-budget", None, None);
        let (inputs, call) = session.guest_inventory().for_diagnostic_call();
        let channel = inputs.diagnostics();
        let diagnostic = ProviderDiagnostic::message(ProviderText::new("trace-storm-canary"));
        for _ in 0..400 {
            channel.emit(ProviderEvent::diagnostic(
                EventLevel::Trace,
                "storm".into(),
                &diagnostic,
            ));
        }
        for (level, message) in [
            (EventLevel::Warn, "warning-after-trace"),
            (EventLevel::Error, "error-after-trace"),
        ] {
            let error = ProviderError::internal(message);
            channel.emit(ProviderEvent::failure(level, "after-storm".into(), &error));
        }
        drop(call);
        return;
    }
    for enabled in [false, true] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "provider_text::event_tests::default_admission_filters_before_separate_level_budgets",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("CARINA_WASI_HTTP_TRACE");
        if enabled {
            command.env("CARINA_WASI_HTTP_TRACE", "1");
        }
        let output = command.output().unwrap();
        let text = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{text}");
        assert!(text.contains("warning-after-trace"), "{text}");
        assert!(text.contains("error-after-trace"), "{text}");
        assert_eq!(text.contains("trace-storm-canary"), enabled, "{text}");
        assert_eq!(
            text.contains("provider events suppressed"),
            enabled,
            "{text}"
        );
    }
}

#[test]
fn custom_sink_observes_trace_without_starving_warnings() {
    let (session, events) = captured_session(ProviderPlan::state_only().session_builder(
        "custom-trace-budget",
        None,
        None,
    ));
    let (inputs, call) = session.guest_inventory().for_diagnostic_call();
    let channel = inputs.diagnostics();
    let diagnostic = ProviderDiagnostic::message(ProviderText::new("custom trace"));
    for _ in 0..400 {
        channel.emit(ProviderEvent::diagnostic(
            EventLevel::Trace,
            "storm".into(),
            &diagnostic,
        ));
    }
    for (level, message) in [
        (EventLevel::Warn, "custom warning after trace"),
        (EventLevel::Error, "custom error after trace"),
    ] {
        let error = ProviderError::internal(message);
        channel.emit(ProviderEvent::failure(level, "after-storm".into(), &error));
    }
    drop(call);
    let text = events
        .lock()
        .unwrap()
        .iter()
        .map(|event| present_event(event).to_string())
        .collect::<Vec<_>>();
    let admitted = text
        .iter()
        .filter(|event| event.contains("custom trace"))
        .count();
    assert!(admitted > 0);
    assert!(
        text.iter()
            .any(|event| event.contains("custom warning after trace"))
    );
    assert!(
        text.iter()
            .any(|event| event.contains("custom error after trace"))
    );
    assert_eq!(
        text.iter()
            .filter(|event| event.contains("provider events suppressed"))
            .collect::<Vec<_>>(),
        vec![&format!("{} provider events suppressed", 400 - admitted)],
    );
}

#[test]
fn default_sink_filters_trace_with_existing_opt_in() {
    const CHILD: &str = "CARINA_4334_TRACE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let session = ProviderPlan::state_only().session("trace-provider", None, None);
        for (level, message) in [
            (EventLevel::Warn, "warn-canary"),
            (EventLevel::Trace, "trace-canary"),
        ] {
            let error = ProviderError::internal(message);
            session
                .diagnostics()
                .emit(ProviderEvent::failure(level, "filter".into(), &error));
        }
        return;
    }
    for enabled in [false, true] {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "provider_text::event_tests::default_sink_filters_trace_with_existing_opt_in",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env_remove("CARINA_WASI_HTTP_TRACE");
        if enabled {
            command.env("CARINA_WASI_HTTP_TRACE", "1");
        }
        let output = command.output().unwrap();
        let text = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{text}");
        assert!(text.contains("warn-canary"), "{text}");
        assert_eq!(text.contains("trace-canary"), enabled, "{text}");
    }
}

#[test]
fn validation_events_render_the_actual_provider_name() {
    let value: crate::resource::Value =
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::String("ordinary".into()));
    let (session, events) = captured_session(ProviderPlan::state_only().session_builder(
        "validation-provider",
        None,
        None,
    ));
    let inputs = boundary::ProviderValidationInputs::collect("validation-provider", [&value])
        .inherit_diagnostic_host(&session.guest_inputs());
    let error = ProviderError::internal("validation event");
    inputs.diagnostics().emit(ProviderEvent::failure(
        EventLevel::Warn,
        "validation".into(),
        &error,
    ));
    let text = present_event(&events.lock().unwrap()[0]).to_string();
    assert!(text.contains("[validation-provider]"), "{text}");
}

#[test]
fn closed_context_registration_does_not_consume_inventory_budget() {
    use carina_provider_protocol::diagnostics::{ProtectedMaterial, RegistrationError};
    let session = ProviderPlan::state_only().session("closed", None, None);
    let channel = session.diagnostics();
    let inventory = session.redaction_set().clone();
    drop(session);
    for _ in 0..2000 {
        assert_eq!(
            channel.register_protected(ProtectedMaterial::GithubToken("closed-token".into())),
            Err(RegistrationError::ClosedContext)
        );
    }
    assert!(inventory.snapshot().is_empty());
    let next = ProviderPlan::state_only().session("next", None, None);
    next.diagnostics()
        .inherit_host(&channel)
        .register_protected(ProtectedMaterial::GithubToken("fresh".repeat(50_000)))
        .unwrap();
}

#[test]
fn handled_failure_then_success_retains_one_warning_and_native_cause() {
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
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
    let (session, events) = captured_session(context.session_builder());
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
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
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
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
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
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
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
    let (session, events) = captured_session(context.session_builder());
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
    let (session, events) = captured_session(plan.session_builder("mock", None, None));
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
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
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
    let (session, _events) =
        captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
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
    let (session, events) =
        captured_session(ProviderPlan::state_only().session_builder("mock", None, None));
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
    let (session, events) = captured_session(context.session_builder());
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
