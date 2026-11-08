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
        if !self.accept_diagnostic_import() {
            return;
        }
        if let Some(event) = decode_event(event) {
            self.input_inventory()
                .diagnostics()
                .receive_wire(event, wasm_convert::decode_provider_error);
        }
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
            protected::Material::SignedMaterial(protected::SignedMaterial {
                authorization,
                signature,
                query_secret_values,
            }) => wire::ProtectedMaterial::SignedMaterial(wire::SignedMaterial {
                authorization,
                signature,
                query_secret_values,
            }),
        };
        let values: Vec<&str> = match &material {
            wire::ProtectedMaterial::AwsSession(wire::AwsSession {
                access_key_id,
                secret_access_key,
                session_token,
            }) => [
                Some(access_key_id.as_str()),
                Some(secret_access_key.as_str()),
                session_token.as_deref(),
            ]
            .into_iter()
            .flatten()
            .collect(),
            wire::ProtectedMaterial::GithubToken(token) => vec![token.as_str()],
            wire::ProtectedMaterial::SignedMaterial(wire::SignedMaterial {
                authorization,
                signature,
                query_secret_values,
            }) => std::iter::once(authorization.as_str())
                .chain(signature.as_deref())
                .chain(query_secret_values.iter().map(String::as_str))
                .collect(),
        };
        let bytes = values
            .iter()
            .try_fold(0usize, |n, value| n.checked_add(value.len()))
            .ok_or(protected::RegistrationError::LimitExceeded)?;
        if !self.accept_protected_material(bytes, values.len()) {
            return Err(protected::RegistrationError::LimitExceeded);
        }
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
fn decode_event(event: wit::Event) -> Option<wire::Event> {
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
                wit::DiagnosticValue::Typed(value) => decode_value(value)?,
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
    Some(wire::Event {
        severity,
        context: context.map(|wit::UnboundDiagnostic { text }| text),
        body,
    })
}
fn decode_value(value: wit_types::Value) -> Option<(wire::DiagnosticValue, bool)> {
    let typed = match value {
        wit_types::Value::BoolVal(value) => proto::Value::Bool(value),
        wit_types::Value::IntVal(value) => proto::Value::Int(value),
        wit_types::Value::FloatVal(value) => {
            proto::Value::Float(carina_provider_protocol::FiniteF64::new(value).ok()?)
        }
        wit_types::Value::StrVal(value) => proto::Value::String(value),
        wit_types::Value::StringListVal(json)
        | wit_types::Value::ListVal(json)
        | wit_types::Value::MapVal(json) => {
            return Some((wire::DiagnosticValue::Json(json), false));
        }
        wit_types::Value::SecretVal(json) => {
            return Some((wire::DiagnosticValue::Json(json), true));
        }
    };
    Some((wire::DiagnosticValue::Typed(typed), false))
}
