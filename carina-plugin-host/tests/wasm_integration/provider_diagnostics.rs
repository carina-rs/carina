use super::*;
use carina_core::provider::{ProviderBoundary, ReadOutcome};
use carina_core::provider_text::{BoundProviderEvent, EventLevel, present_event};
use carina_core::test_utils::provider_text::captured_session;

async fn probe(
    mode: &str,
) -> (
    carina_core::provider::HostResult<ReadOutcome>,
    Vec<BoundProviderEvent>,
) {
    probe_calls(&[mode]).await
}

async fn probe_calls(
    modes: &[&str],
) -> (
    carina_core::provider::HostResult<ReadOutcome>,
    Vec<BoundProviderEvent>,
) {
    let credential_probe = modes.iter().any(|mode| {
        matches!(
            *mode,
            "__mock_diag_registered__"
                | "__mock_diag_register_only__"
                | "__mock_diag_emit_registered_only__"
        )
    });
    let (factory, _cache, provider, resource_type) = if credential_probe {
        let wasm = http_wasm_path().expect("HTTP mock WASM fixture is required");
        let (factory, cache) = load_http_factory(&wasm).await;
        (factory, cache, "mock_http", "test.send_phase")
    } else {
        let wasm = wasm_path().expect("mock WASM fixture is required");
        let (factory, cache) = load_factory(&wasm).await;
        (factory, cache, "mock", "test")
    };
    let config = ready_config(&factory, &indexmap::IndexMap::new());
    let resource: Resource =
        Resource::with_provider(provider, resource_type, "diagnostic-address", None)
            .with_attribute(
                "password",
                Value::Deferred(DeferredValue::Secret(
                    Box::new(Value::Concrete(ConcreteValue::String(
                        "input-secret-canary".into(),
                    )))
                    .into(),
                )),
            );
    let plan = provider_text_fixtures::plan_for_desired(std::slice::from_ref(&resource));
    let (session, events) =
        captured_session(plan.test_session_builder(provider, None, Some(&config)));
    let initialized = ProviderBoundary::initialize_for_test(&factory, &session)
        .await
        .unwrap();
    events.lock().unwrap().clear();
    let mut result = None;
    for mode in modes {
        let next = initialized
            .provider()
            .read(
                &resource.id,
                &AppliedIdentifier::new(*mode).unwrap(),
                ReadRequest,
            )
            .await;
        if let Some(previous) = result.replace(next) {
            assert!(previous.is_ok(), "earlier fixture call must succeed");
        }
    }
    let result = result.expect("at least one fixture call");
    drop(initialized);
    let events = std::mem::take(&mut *events.lock().unwrap());
    (result, events)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handled_failure_is_one_bound_warning_even_when_the_guest_succeeds() {
    let (result, events) = probe("__mock_diag_handled__").await;
    assert!(result.is_ok());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].severity(), EventLevel::Warn);
    let text = present_event(&events[0]).to_string();
    assert!(text.starts_with("warn [mock]"), "{text}");
    for expected in [
        "attempt 2 of 5",
        "handled operation",
        "nested ordinary cause",
        "CreateWidget",
        "429",
        "SlowDown",
        "request-123",
        "(secret)",
    ] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }
    assert!(!text.contains("input-secret-canary"));
}

async fn check_panic(mode: &str, expected: &str) {
    let (result, events) = probe(mode).await;
    assert!(result.is_err());
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].severity(), EventLevel::Error);
    let text = present_event(&events[0]).to_string();
    assert!(text.contains(expected), "{text}");
    assert!(
        text.contains("main.rs"),
        "panic location is missing: {text}"
    );
    assert!(!text.contains("input-secret-canary"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn string_panic_preserves_unbound_payload_and_typed_location() {
    check_panic("__mock_diag_string_panic__", "panic keeps context").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expect_panic_preserves_error_text_without_hook_debug_formatting() {
    check_panic("__mock_diag_expect__", "expect keeps context").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_diagnostic_panic_keeps_its_cause() {
    check_panic("__mock_diag_typed_panic__", "typed panic ordinary cause").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_string_panic_has_location_and_no_payload_text_or_fixed_cause() {
    let (result, events) = probe("__mock_diag_non_string_panic__").await;
    assert!(result.is_err());
    assert_eq!(events.len(), 1);
    let text = present_event(&events[0]).to_string();
    assert!(text.contains("main.rs"));
    for forbidden in [
        "non-string-secret-canary",
        "provider panicked",
        "Box<dyn Any>",
    ] {
        assert!(!text.contains(forbidden), "{text}");
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn emit_commits_before_a_subsequent_wasm_trap() {
    let (result, events) = probe("__mock_diag_emit_then_trap__").await;
    assert!(result.is_err());
    assert_eq!(events.len(), 1);
    assert!(
        present_event(&events[0])
            .to_string()
            .contains("warning survives trap")
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_derived_credentials_are_redacted_in_the_registering_call() {
    let (result, events) = probe("__mock_diag_registered__").await;
    assert!(result.is_ok());
    assert_eq!(events.len(), 1);
    let text = present_event(&events[0]).to_string();
    assert!(text.contains("ordinary registered context"));
    assert!(text.contains("(secret)"));
    for secret in [
        "guest-access-canary",
        "guest-key-canary",
        "guest-token-canary",
        "guest-github-canary",
    ] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guest_derived_credentials_are_redacted_in_the_next_export() {
    let (result, events) = probe_calls(&[
        "__mock_diag_register_only__",
        "__mock_diag_emit_registered_only__",
    ])
    .await;
    assert!(result.is_ok());
    assert_eq!(
        events.len(),
        1,
        "registration and emission must occur in separate calls"
    );
    let text = present_event(&events[0]).to_string();
    assert!(text.contains("ordinary registered context"), "{text}");
    assert_eq!(text.matches("(secret)").count(), 4, "{text}");
    for secret in [
        "guest-access-canary",
        "guest-key-canary",
        "guest-token-canary",
        "guest-github-canary",
    ] {
        assert!(!text.contains(secret), "leaked {secret}: {text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wasm_event_flood_is_bounded_and_does_not_turn_success_into_a_trap() {
    let (result, events) = probe("__mock_diag_budget__").await;
    assert!(result.is_ok());
    assert!(!events.is_empty());
    assert!(events.len() <= 257); // One host suppression notice beyond normal events.
}

#[test]
fn raw_guest_stdio_forwards_zero_bytes_through_all_lifecycle_stages() {
    const CHILD: &str = "CARINA_4334_STDIO_CHILD";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (result, events) = probe("__mock_diag_handled__").await;
                assert!(result.is_ok());
                assert_eq!(events.len(), 1);
            });
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "provider_diagnostics::raw_guest_stdio_forwards_zero_bytes_through_all_lifecycle_stages", "--nocapture"])
        .env(CHILD, "1").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for bytes in [&output.stdout, &output.stderr] {
        let text = String::from_utf8_lossy(bytes);
        for route in ["raw-eprintln-", "raw-stdout-", "raw-writer-", "raw-wasi-"] {
            assert!(!text.contains(route), "raw guest output escaped: {text}");
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_event_budget_never_falls_back_to_raw_panic_output() {
    let (result, events) = probe("__mock_diag_hook_failure__").await;
    assert!(result.is_err());
    assert!(!events.is_empty());
    assert!(events.len() <= 258); // Panic and host suppression notice bypass normal events.
    assert!(events.iter().any(|event| {
        event.severity() == EventLevel::Error
            && present_event(event)
                .to_string()
                .contains("budget-panic-raw-fallback-canary")
    }));
}
