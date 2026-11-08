use super::*;
use std::io::Write;
use std::sync::{Arc, Mutex};

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
            .session("raw-test", None, None)
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
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = carina_core::provider::ProviderPlan::state_only()
        .session("mock", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
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
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let session = carina_core::provider::ProviderPlan::state_only()
        .session("mock", None, None)
        .with_diagnostic_sink(move |event| sink.lock().unwrap().push(event));
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
async fn legacy_wit_package_is_rejected_explicitly_before_instantiation() {
    let engine = Engine::new(&build_engine_config()).unwrap();
    let component = Component::new(
        &engine,
        r#"(component (instance $old) (export "carina:provider/provider@0.1.0" (instance $old)))"#,
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
            assert!(message.as_str().contains("0.2.0"))
        }
        _ => panic!("old WIT must be a typed protocol incompatibility"),
    }
}
