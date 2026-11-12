use super::*;
use carina_core::test_utils::provider_text::captured_session;
use std::io::Write;
use std::sync::{Arc, Mutex};

pub(super) fn diagnostic_store(engine: &Engine) -> GuestStore {
    let identity = ProviderLoadIdentity::for_test("mock", ProviderTrustClass::Untrusted);
    GuestStore::new(
        engine,
        Arc::new(ProviderLabel {
            name: "mock".into(),
            source: identity.source().clone(),
        }),
        GuestNetwork::SchemaOnly,
        None,
        build_store_limits(),
        GuestCallBudget::PRODUCTION,
    )
}

fn emit_wire(store: &mut Store<HostState>, body: carina_provider_protocol::diagnostics::Payload) {
    use carina_provider_protocol::diagnostics as wire;
    store.data().input_inventory().diagnostics().receive_wire(
        wire::Event {
            severity: wire::Level::Warn,
            context: None,
            body,
        },
        wasm_convert::decode_provider_error,
    );
}

#[tokio::test]
async fn panic_flood_is_bounded_and_exemption_resets_each_call() {
    use carina_provider_protocol::diagnostics as wire;
    let (session, events) = captured_session(
        carina_core::provider::ProviderPlan::state_only().test_session_builder("mock", None, None),
    );
    let engine = Engine::default();
    let mut store = diagnostic_store(&engine);
    store.set_configuration_secrets(session.guest_inputs());
    store
        .call(|store| async move {
            for _ in 0..2048 {
                emit_wire(
                    store,
                    wire::Payload::Panic(wire::PanicDiagnostic {
                        message: Some("panic-flood-canary".into()),
                        file: Some("flood.rs".into()),
                        line: Some(1),
                        column: Some(2),
                    }),
                );
            }
        })
        .await
        .unwrap();
    {
        let text = events
            .lock()
            .unwrap()
            .iter()
            .map(|event| carina_core::provider_text::present_event(event).to_string())
            .collect::<Vec<_>>();
        let admitted = text
            .iter()
            .filter(|event| event.contains("panic-flood-canary"))
            .count();
        assert!(
            (1..=257).contains(&admitted),
            "admitted {admitted} panic events"
        );
        assert_eq!(
            text.iter()
                .filter(|event| event.contains("provider events suppressed"))
                .collect::<Vec<_>>(),
            vec![&format!("{} provider events suppressed", 2048 - admitted)],
        );
    }
    events.lock().unwrap().clear();
    store
        .call(|store| async move {
            for _ in 0..300 {
                emit_wire(store, wire::Payload::Diagnostic("ordinary warning".into()));
            }
            emit_wire(
                store,
                wire::Payload::Panic(wire::PanicDiagnostic {
                    message: Some("first panic in next call".into()),
                    file: Some("next-call.rs".into()),
                    line: Some(7),
                    column: Some(3),
                }),
            );
        })
        .await
        .unwrap();
    let events = events.lock().unwrap();
    let panic = events
        .iter()
        .find(|event| {
            carina_core::provider_text::present_event(event)
                .to_string()
                .contains("first panic in next call")
        })
        .expect("the first real panic after a warning flood must remain exempt in each call");
    assert!(
        carina_core::provider_text::present_event(panic)
            .to_string()
            .contains("next-call.rs:7:3")
    );
}

#[tokio::test]
async fn oversized_panic_text_is_masked_without_losing_the_event() {
    use carina_provider_protocol::diagnostics as wire;
    let (session, events) = captured_session(
        carina_core::provider::ProviderPlan::state_only().test_session_builder("mock", None, None),
    );
    let engine = Engine::default();
    let mut store = diagnostic_store(&engine);
    store.set_configuration_secrets(session.guest_inputs());
    for field in 0..3 {
        let oversized = format!("oversized-panic-canary{}", "多".repeat(90_000));
        let (context, message, file) = match field {
            0 => (
                Some(oversized),
                Some("panic".into()),
                Some("panic.rs".into()),
            ),
            1 => (None, Some(oversized), Some("panic.rs".into())),
            2 => (None, Some("panic".into()), Some(oversized)),
            _ => unreachable!(),
        };
        store
            .call(|store| async move {
                store.data().input_inventory().diagnostics().receive_wire(
                    wire::Event {
                        severity: wire::Level::Error,
                        context,
                        body: wire::Payload::Panic(wire::PanicDiagnostic {
                            message,
                            file,
                            line: Some(7),
                            column: Some(3),
                        }),
                    },
                    wasm_convert::decode_provider_error,
                );
            })
            .await
            .unwrap();
    }
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 3);
    for event in &*events {
        let text = carina_core::provider_text::present_event(event).to_string();
        assert!(!text.contains("oversized-panic-canary"), "{text}");
        assert!(text.contains("(secret)"), "{text}");
        assert!(text.contains(":7:3"), "{text}");
    }
}

#[tokio::test]
async fn a_later_call_delivers_panic_message_and_location_after_a_flood() {
    use carina_provider_protocol::diagnostics as wire;
    let (session, events) = captured_session(
        carina_core::provider::ProviderPlan::state_only().test_session_builder("mock", None, None),
    );
    let engine = Engine::default();
    let mut store = diagnostic_store(&engine);
    store.set_configuration_secrets(session.guest_inputs());
    store
        .call(|store| async move {
            for _ in 0..300 {
                emit_wire(store, wire::Payload::Diagnostic("ordinary warning".into()));
            }
        })
        .await
        .unwrap();
    events.lock().unwrap().clear();
    store
        .call(|store| async move {
            emit_wire(
                store,
                wire::Payload::Diagnostic("later normal warning".into()),
            );
            emit_wire(
                store,
                wire::Payload::Panic(wire::PanicDiagnostic {
                    message: Some("later panic".into()),
                    file: Some("provider.rs".into()),
                    line: Some(41),
                    column: Some(2),
                }),
            );
        })
        .await
        .unwrap();
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 2);
    let text = carina_core::provider_text::present_event(&events[1]).to_string();
    assert!(text.contains("later panic"), "{text}");
    assert!(text.contains("provider.rs:41:2"), "{text}");
}

#[tokio::test]
async fn call_end_reports_exactly_one_suppression_count_and_keeps_the_panic() {
    use carina_provider_protocol::diagnostics as wire;
    let (session, events) = captured_session(
        carina_core::provider::ProviderPlan::state_only().test_session_builder("mock", None, None),
    );
    let engine = Engine::default();
    let mut store = diagnostic_store(&engine);
    store.set_configuration_secrets(session.guest_inputs());
    store
        .call(|store| async move {
            for _ in 0..300 {
                emit_wire(store, wire::Payload::Diagnostic("ordinary warning".into()));
            }
            emit_wire(
                store,
                wire::Payload::Panic(wire::PanicDiagnostic {
                    message: Some("panic beyond budget".into()),
                    file: Some("panic.rs".into()),
                    line: Some(7),
                    column: Some(3),
                }),
            );
        })
        .await
        .unwrap();
    let events = events.lock().unwrap();
    let text = events
        .iter()
        .map(|event| carina_core::provider_text::present_event(event).to_string())
        .collect::<Vec<_>>();
    let admitted = text
        .iter()
        .filter(|s| s.contains("ordinary warning"))
        .count();
    let notices = text
        .iter()
        .filter(|s| s.contains("provider events suppressed"))
        .collect::<Vec<_>>();
    assert_eq!(notices.len(), 1, "{text:?}");
    assert!(
        notices[0].contains(&format!("{} provider events suppressed", 300 - admitted)),
        "{notices:?}"
    );
    assert_eq!(
        text.iter()
            .filter(|s| s.contains("panic beyond budget"))
            .count(),
        1
    );
}

#[tokio::test]
async fn metadata_fallback_retains_the_installed_sink_and_schemas() {
    use carina_core::provider::ProviderBoundary;
    use carina_core::provider_text::{
        EventLevel, ProviderEvent, ProviderValueDiagnostic, ValueOrigin,
    };
    use carina_core::resource::{ConcreteValue, Resource, Value};
    let resource: Resource = Resource::with_provider("mock", "test.resource", "metadata", None)
        .with_attribute(
            "name",
            Value::Concrete(ConcreteValue::String("ordinary".into())),
        );
    let plan = crate::provider_text_fixtures::plan_for_desired(&[resource]);
    let (session, events) = captured_session(plan.test_session_builder("mock", None, None));
    let factory = carina_provider_mock::ConfiguredBuiltinMock::new(
        Default::default(),
        carina_provider_mock::MockScope::memory(),
    );
    let _normalizer = ProviderBoundary::initialize_normalizer_for_test(&factory, &session)
        .await
        .unwrap();
    let engine = Engine::default();
    let mut store = diagnostic_store(&engine);
    store.set_input_inventory(session.guest_inventory());
    for round in 0..2 {
        if round == 1 {
            store.set_input_inventory(plan.test_session("mock", None, None).guest_inventory());
        }
        store
            .call(|store| async move {
                let error = ProviderError::internal("metadata warning");
                let channel = store.data().input_inventory().diagnostics();
                channel.emit(ProviderEvent::failure(
                    EventLevel::Warn,
                    "metadata".into(),
                    &error,
                ));
                let id = proto::ResourceId {
                    provider: "mock".into(),
                    resource_type: "test.resource".into(),
                    identity: "metadata".into(),
                };
                channel.emit(ProviderEvent::value(
                    EventLevel::Warn,
                    ProviderValueDiagnostic::typed(
                        Some(&id),
                        &["name".into()],
                        ValueOrigin::Observed,
                        &proto::Value::String("public metadata value".into()),
                    ),
                ));
            })
            .await
            .unwrap();
    }
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 4);
    for [warning, value] in events.as_chunks::<2>().0 {
        assert!(
            carina_core::provider_text::present_event(warning)
                .to_string()
                .contains("metadata warning")
        );
        assert!(
            carina_core::provider_text::present_event(value)
                .to_string()
                .contains("public metadata value")
        );
    }
}

#[test]
fn http_fields_are_structurally_masked_without_registered_material() {
    use carina_provider_protocol::diagnostics as wire;
    let (session, events) = captured_session(
        carina_core::provider::ProviderPlan::state_only().test_session_builder("mock", None, None),
    );
    session.diagnostics().receive_wire(wire::Event {
        severity: wire::Level::Trace, context: None,
        body: wire::Payload::Http(wire::HttpDiagnostic {
            method: "GET".into(), uri: "https://example.com/?X-Amz-Signature=signature-canary&access_token=token-canary".into(),
            headers: vec![("AUTHORIZATION".into(), b"authorization-canary".to_vec()), ("Set-Cookie".into(), b"cookie-canary".to_vec())],
            request_body_bytes: 0, response_body_bytes: None, status: None, transport_code: None, phases_ms: [0; 7],
        }),
    }, wasm_convert::decode_provider_error);
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let text = carina_core::provider_text::present_event(&events[0]).to_string();
    for secret in [
        "signature-canary",
        "token-canary",
        "authorization-canary",
        "cookie-canary",
    ] {
        assert!(!text.contains(secret), "{text}");
    }
    assert!(text.contains("<redacted>"));
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn raw_wasi_stdout_and_stderr_have_no_outward_sink_at_any_lifecycle_stage() {
    use wasmtime_wasi::cli::WasiCliCtxView;
    use wasmtime_wasi::p2::bindings::cli::{stderr, stdout};
    for stage in ["metadata", "initialization", "runtime", "teardown"] {
        let capture = Capture::default();
        let host_output = guest_stderr::HostOutput::new(capture.clone());
        let engine = Engine::default();
        let inventory = carina_core::provider::ProviderPlan::state_only()
            .test_session("raw-test", None, None)
            .guest_inventory();
        let owner = carina_core::provider_text::GuestStoreData::new_store(&engine, (), inventory);
        let mut wasi = build_sandboxed_wasi_ctx_for_store(owner.data(), None, |_key| None);
        let mut table = wasmtime::component::ResourceTable::new();
        let mut cli = WasiCliCtxView {
            ctx: wasi.cli(),
            table: &mut table,
        };
        let err = stderr::Host::get_stderr(&mut cli).unwrap();
        let out = stdout::Host::get_stdout(&mut cli).unwrap();
        for stream in [err, out] {
            let stream = cli.table.get_mut(&stream).unwrap();
            stream
                .write(hyper::body::Bytes::from(format!("raw-{stage}-canary\n")))
                .unwrap();
            stream.flush().unwrap();
        }
        drop(wasi);
        assert!(
            capture.0.lock().unwrap().is_empty(),
            "raw output forwarded during {stage}"
        );
        host_output
            .write_line("host-owned output remains available")
            .unwrap();
        assert!(
            String::from_utf8(capture.0.lock().unwrap().clone())
                .unwrap()
                .contains("host-owned output")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sdk_hook_precedes_provider_construction_during_metadata_loading() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../target/wasm32-wasip2/debug/carina-provider-mock-constructor-panic.wasm");
    let engine = Engine::new(&build_engine_config()).unwrap();
    let component = Component::from_file(&engine, path).unwrap();
    let identity = ProviderLoadIdentity::for_test("mock", ProviderTrustClass::Untrusted);
    let label = Arc::new(ProviderLabel {
        name: "mock".into(),
        source: identity.source().clone(),
    });
    let (session, events) = captured_session(
        carina_core::provider::ProviderPlan::state_only().test_session_builder("mock", None, None),
    );
    let result = create_instance(
        &engine,
        &component,
        label,
        GuestNetwork::SchemaOnly,
        GuestCallBudget::PRODUCTION,
        Some(session.guest_inputs()),
    )
    .await;
    assert!(result.is_err(), "constructor must trap before info returns");
    let events = events.lock().unwrap();
    assert_eq!(events.len(), 1);
    let text = carina_core::provider_text::present_event(&events[0]).to_string();
    assert!(text.contains("constructor keeps context"), "{text}");
    assert!(text.contains("constructor_panic.rs"), "{text}");
    assert!(!text.contains("constructor-secret-canary"), "{text}");
}

#[test]
fn masked_http_fields_still_consume_the_input_size_budget() {
    use carina_provider_protocol::diagnostics as wire;
    let (session, events) = captured_session(
        carina_core::provider::ProviderPlan::state_only().test_session_builder("mock", None, None),
    );
    session.diagnostics().receive_wire(
        wire::Event {
            severity: wire::Level::Trace,
            context: None,
            body: wire::Payload::Http(wire::HttpDiagnostic {
                method: "GET".into(),
                uri: "https://example.com/".into(),
                headers: vec![("authorization".into(), vec![b'x'; 100_000])],
                request_body_bytes: 0,
                response_body_bytes: None,
                status: None,
                transport_code: None,
                phases_ms: [0; 7],
            }),
        },
        wasm_convert::decode_provider_error,
    );
    assert!(events.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_wit_package_is_rejected_explicitly_before_current_world_binding() {
    let engine = Engine::new(&build_engine_config()).unwrap();
    for version in ["0.1.0", "0.2.0"] {
        let component = Component::new(
            &engine,
            format!(
                r#"(component (instance $old) (export "carina:provider/provider@{version}" (instance $old)))"#,
            ),
        )
        .unwrap();
        let identity = ProviderLoadIdentity::for_test("mock", ProviderTrustClass::Untrusted);
        let label = Arc::new(ProviderLabel {
            name: "mock".into(),
            source: identity.source().clone(),
        });
        let result = create_instance(
            &engine,
            &component,
            label,
            GuestNetwork::SchemaOnly,
            GuestCallBudget::PRODUCTION,
            None,
        )
        .await;
        match result {
            Err(WasmBindingCreationError::ProtocolVersion(message)) => {
                assert!(message.as_str().contains("0.3.0"))
            }
            _ => panic!("old WIT must be a typed protocol incompatibility"),
        }
    }
}

#[tokio::test]
async fn legacy_aws_import_is_rejected_before_linking() {
    let engine = Engine::new(&build_engine_config()).unwrap();
    let component = Component::new(
        &engine,
        r#"(component
        (type $credentials (instance (export "current-credentials" (func))))
        (import "carina:provider/aws-credentials@0.1.0" (instance (type $credentials)))
        (instance $old)
        (export "carina:provider/provider@0.1.0" (instance $old)))"#,
    )
    .unwrap();
    let mut store = diagnostic_store(&engine);
    let linker = create_linker(
        &engine,
        &GuestNetwork::SchemaOnly,
        InstantiationWorld::Basic,
    )
    .unwrap_or_else(|_| panic!("test linker must be constructible"));
    let result = WasmBindings::instantiate_basic(&mut store, &component, &linker).await;
    match result {
        Err(WasmBindingCreationError::ProtocolVersion(message)) => {
            let text = message.as_str();
            assert!(text.contains("carina:provider@0.1.0"), "{text}");
            assert!(text.contains("carina:provider@0.3.0"), "{text}");
            assert!(
                text.contains(&format!(
                    "protocol {}",
                    carina_provider_protocol::PROTOCOL_VERSION
                )),
                "{text}"
            );
        }
        _other => panic!(
            "legacy imports must produce the typed WIT incompatibility before linker failure"
        ),
    }
}

#[tokio::test]
async fn legacy_start_executes_zero_guest_code() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for version in ["0.1.0", "0.2.0"] {
        let engine = Engine::new(&build_engine_config()).unwrap();
        let component = Component::new(
            &engine,
            format!(
                r#"(component
        (import "probe" (func $probe))
        (core func $lower (canon lower (func $probe)))
        (core module $m
            (import "" "probe" (func $probe))
            (func $start call $probe)
            (start $start))
        (core instance $imports (export "probe" (func $lower)))
        (core instance $run (instantiate $m (with "" (instance $imports))))
        (instance $old)
        (export "carina:provider/provider@{version}" (instance $old)))"#
            ),
        )
        .unwrap();
        let mut store = diagnostic_store(&engine);
        let mut linker = create_linker(
            &engine,
            &GuestNetwork::SchemaOnly,
            InstantiationWorld::Basic,
        )
        .unwrap_or_else(|_| panic!("test linker must be constructible"));
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        linker
            .root()
            .func_wrap("probe", move |_store, (): ()| {
                called.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .unwrap();
        let result = WasmBindings::instantiate_basic(&mut store, &component, &linker).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "legacy component start ran before rejection"
        );
        match result {
            Err(WasmBindingCreationError::ProtocolVersion(message)) => {
                assert!(
                    message
                        .as_str()
                        .contains(&format!("carina:provider@{version}"))
                );
                assert!(message.as_str().contains("carina:provider@0.3.0"));
                assert!(message.as_str().contains(&format!(
                    "protocol {}",
                    carina_provider_protocol::PROTOCOL_VERSION
                )));
            }
            _other => panic!("a legacy component must be rejected before instantiation"),
        }
    }
}

async fn assert_wit_remediation(version: &str, remedy: &str) {
    let engine = Engine::new(&build_engine_config()).unwrap();
    let component = Component::new(&engine, format!(r#"(component (instance $provider) (export "carina:provider/provider@{version}" (instance $provider)))"#)).unwrap();
    let mut store = diagnostic_store(&engine);
    let linker = create_linker(
        &engine,
        &GuestNetwork::SchemaOnly,
        InstantiationWorld::Basic,
    )
    .unwrap_or_else(|_error| panic!("test linker must be constructible"));
    match WasmBindings::instantiate_basic(&mut store, &component, &linker).await {
        Err(WasmBindingCreationError::ProtocolVersion(message)) => {
            let text = message.as_str();
            assert!(
                text.contains(&format!("carina:provider@{version}")),
                "{text}"
            );
            assert!(text.contains("carina:provider@0.3.0"), "{text}");
            assert!(
                text.contains(&format!(
                    "protocol {}",
                    carina_provider_protocol::PROTOCOL_VERSION
                )),
                "{text}"
            );
            assert!(text.contains(remedy), "{text}");
        }
        _other => panic!("WIT mismatch must be typed and rejected before linker/binding errors"),
    }
}

#[tokio::test]
async fn older_wit_package_explains_provider_rebuild_before_linking() {
    for version in ["0.1.0", "0.2.0"] {
        assert_wit_remediation(
            version,
            "rebuild the provider against the current carina protocol",
        )
        .await;
    }
}
#[tokio::test]
async fn newer_wit_package_explains_host_upgrade_before_linking() {
    assert_wit_remediation("0.4.0", "upgrade Carina").await;
}
