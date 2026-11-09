//! Untrusted diagnostic and confidential-registration imports stay in confinement.
use super::version_checked_bindings::{
    wit_diagnostics as wit, wit_protected_material as protected,
};
use super::*;
use carina_provider_protocol::diagnostics as wire;

pub(in crate::wasm_factory) fn add_diagnostic_imports(
    linker: &mut Linker<HostState>,
) -> wasmtime::Result<()> {
    wit::add_to_linker::<HostState, wasmtime::component::HasSelf<_>>(linker, |state| state)?;
    protected::add_to_linker::<HostState, wasmtime::component::HasSelf<_>>(linker, |state| state)
}

impl wit::Host for HostState {
    fn emit(&mut self, event: wit::Event) {
        self.input_inventory()
            .diagnostics()
            .receive_wire(decode_event(event), wasm_convert::decode_provider_error);
    }
}
impl protected::Host for HostState {
    fn register(
        &mut self,
        material: protected::Material,
    ) -> Result<(), protected::RegistrationError> {
        let material = match material {
            protected::Material::AwsSession(protected::AwsSession {
                access_key_id,
                secret_access_key,
                session_token,
            }) => wire::ProtectedMaterial::AwsSession(wire::AwsSession {
                access_key_id,
                secret_access_key,
                session_token,
            }),
            protected::Material::GithubToken(token) => wire::ProtectedMaterial::GithubToken(token),
        };
        self.input_inventory()
            .diagnostics()
            .register_protected(material)
            .map_err(|error| match error {
                wire::RegistrationError::LimitExceeded => {
                    protected::RegistrationError::LimitExceeded
                }
                wire::RegistrationError::ClosedContext => {
                    protected::RegistrationError::ClosedContext
                }
            })
    }
}
fn decode_event(event: wit::Event) -> wire::Event {
    let wit::Event {
        severity,
        context,
        body,
    } = event;
    let severity = match severity {
        wit::Level::Error => wire::Level::Error,
        wit::Level::Warn => wire::Level::Warn,
        wit::Level::Info => wire::Level::Info,
        wit::Level::Debug => wire::Level::Debug,
        wit::Level::Trace => wire::Level::Trace,
    };
    let body = match body {
        wit::Payload::Failure(error) => {
            wire::Payload::Failure(wasm_convert::wit_to_wire_provider_error(error))
        }
        wit::Payload::Diagnostic(wit::UnboundDiagnostic { text }) => {
            wire::Payload::Diagnostic(text)
        }
        wit::Payload::Panic(wit::PanicDiagnostic {
            message,
            file,
            line,
            column,
        }) => wire::Payload::Panic(wire::PanicDiagnostic {
            message: message.map(|wit::UnboundDiagnostic { text }| text),
            file,
            line,
            column,
        }),
        wit::Payload::Value(wit::ValueDiagnostic {
            id,
            path,
            origin,
            input,
        }) => {
            let (input, secret_hint) = match input {
                wit::DiagnosticValue::Json(json) => (wire::DiagnosticValue::Json(json), false),
                wit::DiagnosticValue::Typed(value) => decode_value(value),
            };
            let origin = match origin {
                wit::ValueOrigin::Desired => wire::ValueOrigin::Desired,
                wit::ValueOrigin::Observed => wire::ValueOrigin::Observed,
                wit::ValueOrigin::Unclassified => wire::ValueOrigin::Unclassified,
            };
            wire::Payload::Value(wire::ValueDiagnostic {
                id: id.map(
                    |wit_types::ResourceId {
                         provider,
                         resource_type,
                         identity,
                     }| proto::ResourceId {
                        provider,
                        resource_type,
                        identity,
                    },
                ),
                path,
                origin: if secret_hint {
                    wire::ValueOrigin::Unclassified
                } else {
                    origin
                },
                input,
            })
        }
        wit::Payload::Http(wit::HttpDiagnostic {
            method,
            uri,
            headers,
            request_body_bytes,
            response_body_bytes,
            status,
            transport_code,
            phases_ms,
        }) => {
            let wit::HttpTimings {
                setup,
                handle,
                write_body,
                body_finish,
                pollable_block,
                get_response,
                read_body,
            } = phases_ms;
            wire::Payload::Http(wire::HttpDiagnostic {
                method,
                uri,
                headers,
                request_body_bytes,
                response_body_bytes,
                status,
                transport_code: transport_code.map(|code| match code {
                    wit::HttpTransportCode::Dns => wire::HttpTransportCode::Dns,
                    wit::HttpTransportCode::Connect => wire::HttpTransportCode::Connect,
                    wit::HttpTransportCode::Tls => wire::HttpTransportCode::Tls,
                    wit::HttpTransportCode::Timeout => wire::HttpTransportCode::Timeout,
                    wit::HttpTransportCode::Protocol => wire::HttpTransportCode::Protocol,
                    wit::HttpTransportCode::Body => wire::HttpTransportCode::Body,
                }),
                phases_ms: [
                    setup,
                    handle,
                    write_body,
                    body_finish,
                    pollable_block,
                    get_response,
                    read_body,
                ],
            })
        }
    };
    wire::Event {
        severity,
        context: context.map(|wit::UnboundDiagnostic { text }| text),
        body,
    }
}
fn decode_value(value: wit_types::Value) -> (wire::DiagnosticValue, bool) {
    let typed = match value {
        wit_types::Value::BoolVal(value) => proto::Value::Bool(value),
        wit_types::Value::IntVal(value) => proto::Value::Int(value),
        wit_types::Value::FloatVal(value) => {
            match carina_provider_protocol::FiniteF64::new(value) {
                Ok(value) => proto::Value::Float(value),
                Err(_non_finite) => return (wire::DiagnosticValue::Masked, false),
            }
        }
        wit_types::Value::StrVal(value) => proto::Value::String(value),
        wit_types::Value::StringListVal(json)
        | wit_types::Value::ListVal(json)
        | wit_types::Value::MapVal(json) => {
            return (wire::DiagnosticValue::Json(json), false);
        }
        wit_types::Value::SecretVal(json) => {
            return (wire::DiagnosticValue::Json(json), true);
        }
    };
    (wire::DiagnosticValue::Typed(typed), false)
}

#[cfg(test)]
mod tests {
    use super::super::diagnostic_tests::diagnostic_store;
    use super::*;
    use carina_core::test_utils::provider_text::captured_session;

    #[tokio::test]
    async fn rejected_registration_does_not_consume_a_second_host_budget() {
        use carina_provider_protocol::diagnostics::ProtectedMaterial;
        let (session, _events) = captured_session(
            carina_core::provider::ProviderPlan::state_only().session_builder("mock", None, None),
        );
        let engine = Engine::default();
        let mut store = diagnostic_store(&engine);
        store.set_configuration_secrets(session.guest_inputs());
        store
            .call(|store| async move {
                store
                    .data()
                    .input_inventory()
                    .diagnostics()
                    .register_protected(ProtectedMaterial::GithubToken("x".repeat(250_000)))
                    .unwrap();
                for _ in 0..13 {
                    assert!(matches!(
                        <HostState as protected::Host>::register(
                            store.data_mut(),
                            protected::Material::GithubToken("y".repeat(20_000))
                        ),
                        Err(protected::RegistrationError::LimitExceeded)
                    ));
                }
                assert!(
                    <HostState as protected::Host>::register(
                        store.data_mut(),
                        protected::Material::GithubToken("z".repeat(3_000))
                    )
                    .is_ok(),
                    "rejected registrations charged a duplicate host budget"
                );
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn non_finite_value_masks_its_leaf_and_keeps_the_event() {
        let (session, events) = captured_session(
            carina_core::provider::ProviderPlan::state_only().session_builder("mock", None, None),
        );
        let engine = Engine::default();
        let event = wit::Event {
            severity: wit::Level::Warn,
            context: Some(wit::UnboundDiagnostic {
                text: "float diagnostic".into(),
            }),
            body: wit::Payload::Value(wit::ValueDiagnostic {
                id: None,
                path: vec![],
                origin: wit::ValueOrigin::Observed,
                input: wit::DiagnosticValue::Typed(wit_types::Value::FloatVal(f64::NAN)),
            }),
        };
        // Use an actual HostState so this exercises the confined WIT decoder.
        let mut store = diagnostic_store(&engine);
        store.set_configuration_secrets(session.guest_inputs());
        store
            .call(|state| {
                <HostState as wit::Host>::emit(state.data_mut(), event);
                std::future::ready(())
            })
            .await
            .unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        let text = carina_core::provider_text::present_event(&events[0]).to_string();
        assert!(text.contains("float diagnostic"));
        assert!(text.contains("(secret)"));
    }

    #[tokio::test]
    async fn registered_credentials_survive_call_and_set_secrets_swap() {
        for swap in [false, true] {
            let engine = Engine::default();
            let (session, events) = captured_session(
                carina_core::provider::ProviderPlan::state_only()
                    .session_builder("mock", None, None),
            );
            let mut store = diagnostic_store(&engine);
            store.set_configuration_secrets(session.guest_inputs());
            store
                .call(|store| async move {
                    assert!(
                        protected::Host::register(
                            store.data_mut(),
                            protected::Material::AwsSession(protected::AwsSession {
                                access_key_id: "prior-call-access".into(),
                                secret_access_key: "prior-call-key".into(),
                                session_token: Some("prior-call-session".into()),
                            })
                        )
                        .is_ok()
                    );
                    assert!(
                        protected::Host::register(
                            store.data_mut(),
                            protected::Material::GithubToken("prior-call-github".into())
                        )
                        .is_ok()
                    );
                })
                .await
                .unwrap();
            assert!(events.lock().unwrap().is_empty());
            let replacement =
                carina_core::provider::ProviderPlan::state_only().session("mock", None, None);
            if swap {
                store.set_secrets(replacement.guest_inputs());
            }
            store.call(|store| async move {
                <HostState as wit::Host>::emit(store.data_mut(), wit::Event {
                    severity: wit::Level::Warn,
                    context: None,
                    body: wit::Payload::Diagnostic(wit::UnboundDiagnostic {
                        text: "later-call public=visible prior-call-access prior-call-key prior-call-session prior-call-github".into(),
                    }),
                });
            }).await.unwrap();
            let captured = events.lock().unwrap();
            assert_eq!(captured.len(), 1, "swap={swap}");
            let text = carina_core::provider_text::present_event(&captured[0]).to_string();
            assert!(text.contains("later-call public=visible"), "{text}");
            assert_eq!(text.matches("(secret)").count(), 4, "swap={swap}: {text}");
            for secret in [
                "prior-call-access",
                "prior-call-key",
                "prior-call-session",
                "prior-call-github",
            ] {
                assert!(
                    !text.contains(secret),
                    "swap={swap}, leaked {secret}: {text}"
                );
            }
        }
    }
}
