//! The only diagnostic encoder. Strings here are transport fields, never presentation.
use carina_plugin_bindings_diagnostics::carina::provider::{
    diagnostics as wit, protected_material as protection, types,
};
use carina_provider_protocol::{diagnostics as wire, types as protocol};

pub(super) fn emit(event: wire::Event) {
    let wire::Event {
        severity,
        context,
        body,
    } = event;
    let severity = match severity {
        wire::Level::Error => wit::Level::Error,
        wire::Level::Warn => wit::Level::Warn,
        wire::Level::Info => wit::Level::Info,
        wire::Level::Debug => wit::Level::Debug,
        wire::Level::Trace => wit::Level::Trace,
    };
    let body = match body {
        wire::Payload::Failure(error) => wit::Payload::Failure(error_to_wit(error)),
        wire::Payload::Diagnostic(text) => {
            wit::Payload::Diagnostic(wit::UnboundDiagnostic { text })
        }
        wire::Payload::Panic(wire::PanicDiagnostic {
            message,
            file,
            line,
            column,
        }) => wit::Payload::Panic(wit::PanicDiagnostic {
            message: message.map(|text| wit::UnboundDiagnostic { text }),
            file,
            line,
            column,
        }),
        wire::Payload::Value(wire::ValueDiagnostic {
            id,
            path,
            origin,
            input,
        }) => wit::Payload::Value(wit::ValueDiagnostic {
            id: id.map(id_to_wit),
            path,
            origin: match origin {
                wire::ValueOrigin::Desired => wit::ValueOrigin::Desired,
                wire::ValueOrigin::Observed => wit::ValueOrigin::Observed,
                wire::ValueOrigin::Unclassified => wit::ValueOrigin::Unclassified,
            },
            input: match input {
                wire::DiagnosticValue::Typed(value) => {
                    wit::DiagnosticValue::Typed(value_to_wit(value))
                }
                wire::DiagnosticValue::Json(json) => wit::DiagnosticValue::Json(json),
                wire::DiagnosticValue::Masked => {
                    wit::DiagnosticValue::Typed(types::Value::FloatVal(f64::NAN))
                }
            },
        }),
        wire::Payload::Http(wire::HttpDiagnostic {
            method,
            uri,
            headers,
            request_body_bytes,
            response_body_bytes,
            status,
            transport_code,
            phases_ms,
        }) => {
            let [
                setup,
                handle,
                write_body,
                body_finish,
                pollable_block,
                get_response,
                read_body,
            ] = phases_ms;
            wit::Payload::Http(wit::HttpDiagnostic {
                method,
                uri,
                headers,
                request_body_bytes,
                response_body_bytes,
                status,
                transport_code: transport_code.map(|code| match code {
                    wire::HttpTransportCode::Dns => wit::HttpTransportCode::Dns,
                    wire::HttpTransportCode::Connect => wit::HttpTransportCode::Connect,
                    wire::HttpTransportCode::Tls => wit::HttpTransportCode::Tls,
                    wire::HttpTransportCode::Timeout => wit::HttpTransportCode::Timeout,
                    wire::HttpTransportCode::Protocol => wit::HttpTransportCode::Protocol,
                    wire::HttpTransportCode::Body => wit::HttpTransportCode::Body,
                }),
                phases_ms: wit::HttpTimings {
                    setup,
                    handle,
                    write_body,
                    body_finish,
                    pollable_block,
                    get_response,
                    read_body,
                },
            })
        }
    };
    wit::emit(&wit::Event {
        severity,
        context: context.map(|text| wit::UnboundDiagnostic { text }),
        body,
    });
}
fn id_to_wit(
    protocol::ResourceId {
        provider,
        resource_type,
        identity,
    }: protocol::ResourceId,
) -> types::ResourceId {
    types::ResourceId {
        provider,
        resource_type,
        identity,
    }
}
fn value_to_wit(value: protocol::Value) -> types::Value {
    match value {
        protocol::Value::Bool(value) => types::Value::BoolVal(value),
        protocol::Value::Int(value) => types::Value::IntVal(value),
        protocol::Value::Float(value) => types::Value::FloatVal(value.get()),
        protocol::Value::String(value) => types::Value::StrVal(value),
        protocol::Value::StringList(values) => types::Value::StringListVal(
            serde_json::to_string(&values).expect("string list encodes"),
        ),
        protocol::Value::List(values) => {
            types::Value::ListVal(serde_json::to_string(&values).expect("finite list encodes"))
        }
        protocol::Value::Map(values) => {
            types::Value::MapVal(serde_json::to_string(&values).expect("finite map encodes"))
        }
    }
}
fn error_to_wit(error: protocol::ProviderError) -> types::ProviderError {
    let protocol::ProviderError {
        kind,
        retryability,
        message,
        resource_id,
        cause,
        provider_name,
        operation,
        status,
        code,
        request_id,
    } = error;
    let detail = types::ErrorDetail {
        retryability: crate::wasm_guest::proto_to_wit_retryability(retryability),
        message,
        resource_id: resource_id.map(id_to_wit),
        cause,
        provider_name,
        operation,
        status,
        code,
        request_id,
    };
    match kind {
        protocol::ProviderErrorKind::InvalidInput => types::ProviderError::InvalidInput(detail),
        protocol::ProviderErrorKind::ApiError => types::ProviderError::ApiError(detail),
        protocol::ProviderErrorKind::NotFound => types::ProviderError::NotFound(detail),
        protocol::ProviderErrorKind::Timeout => types::ProviderError::Timeout(detail),
        protocol::ProviderErrorKind::Internal => types::ProviderError::Internal(detail),
    }
}

pub(super) fn register(material: wire::ProtectedMaterial) -> Result<(), wire::RegistrationError> {
    let material = match material {
        wire::ProtectedMaterial::AwsSession(wire::AwsSession {
            access_key_id,
            secret_access_key,
            session_token,
        }) => protection::Material::AwsSession(protection::AwsSession {
            access_key_id,
            secret_access_key,
            session_token,
        }),
        wire::ProtectedMaterial::GithubToken(token) => protection::Material::GithubToken(token),
    };
    protection::register(&material).map_err(|error| match error {
        protection::RegistrationError::LimitExceeded => wire::RegistrationError::LimitExceeded,
        protection::RegistrationError::ClosedContext => wire::RegistrationError::ClosedContext,
    })
}
