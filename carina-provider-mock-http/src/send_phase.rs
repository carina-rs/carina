use std::collections::BTreeMap;

use carina_plugin_sdk::core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, ValidatedResourceSchema,
};
use carina_plugin_sdk::types::{AppliedIdentifier, CreateRequest, ProviderError, Value};
use carina_plugin_sdk::{CreateAttempt, CreateStep, CreateVerdict, PhasedToken, Sent};

const URL_ATTRIBUTE: &str = "url";
const METHOD_ATTRIBUTE: &str = "method";
const BODY_ATTRIBUTE: &str = "body";
const SEND_PHASE_ATTRIBUTE: &str = "send_phase";

#[derive(Clone, Copy)]
pub enum ProbeKind {
    HttpWorld,
    AwsWorld,
}

impl ProbeKind {
    fn description(self) -> &'static str {
        match self {
            Self::HttpWorld => "WASI HTTP send-phase probe",
            Self::AwsWorld => "AWS-world WASI HTTP send-phase probe",
        }
    }

    fn schema_expectation(self) -> &'static str {
        match self {
            Self::HttpWorld => "mock HTTP probe schema must be valid",
            Self::AwsWorld => "mock AWS HTTP probe schema must be valid",
        }
    }

    fn missing_attribute_message(self, attribute: &str) -> String {
        let probe = match self {
            Self::HttpWorld => "the HTTP probe",
            Self::AwsWorld => "the AWS HTTP probe",
        };
        format!("{probe} requires a string {attribute} attribute")
    }

    #[cfg(target_arch = "wasm32")]
    fn runtime_components_expectation(self) -> &'static str {
        match self {
            Self::HttpWorld => "HTTP probe runtime components must be valid",
            Self::AwsWorld => "AWS HTTP probe runtime components must be valid",
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn url_expectation(self) -> &'static str {
        match self {
            Self::HttpWorld => "HTTP probe URL must be valid",
            Self::AwsWorld => "AWS HTTP probe URL must be valid",
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn method_expectation(self) -> &'static str {
        match self {
            Self::HttpWorld => "HTTP probe method must be valid",
            Self::AwsWorld => "AWS HTTP probe method must be valid",
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn checks_connector_error_kind(self) -> bool {
        matches!(self, Self::HttpWorld)
    }

    fn identifier(self) -> &'static str {
        match self {
            Self::HttpWorld => "send-phase-probe",
            Self::AwsWorld => "aws-send-phase-probe",
        }
    }

    fn identifier_expectation(self) -> &'static str {
        match self {
            Self::HttpWorld => "the probe identifier is non-empty",
            Self::AwsWorld => "the AWS HTTP probe identifier is non-empty",
        }
    }
}

pub fn schema(kind: ProbeKind, create_behavior_attribute: Option<&str>) -> ValidatedResourceSchema {
    let expectation = kind.schema_expectation();
    let schema = ResourceSchema::new("test.send_phase")
        .attribute(AttributeSchema::required(URL_ATTRIBUTE, AttributeType::string()).build())
        .attribute(AttributeSchema::required(METHOD_ATTRIBUTE, AttributeType::string()).build())
        .attribute(AttributeSchema::required(BODY_ATTRIBUTE, AttributeType::string()).build())
        .attribute(
            AttributeSchema::provider_populated(SEND_PHASE_ATTRIBUTE, AttributeType::string())
                .build(),
        );
    let schema = if let Some(attribute) = create_behavior_attribute {
        schema.attribute(AttributeSchema::optional(attribute, AttributeType::string()).build())
    } else {
        schema
    };
    schema
        .with_description(kind.description())
        .validate()
        .expect(expectation)
}

pub struct PrepareError {
    attempt: CreateAttempt,
    message: String,
}

impl PrepareError {
    pub fn untracked<T>(self, error: impl FnOnce(String) -> ProviderError) -> CreateVerdict<T> {
        self.attempt.untracked(error(self.message))
    }
}

pub struct PreparedCreate {
    attempt: CreateAttempt,
    request: CreateRequest,
    kind: ProbeKind,
    url: String,
    method: String,
    body: String,
}

impl PreparedCreate {
    pub fn prepare(
        request: CreateRequest,
        attempt: CreateAttempt,
        kind: ProbeKind,
    ) -> Result<Self, PrepareError> {
        let attributes = &request.resource.attributes;
        let missing = [URL_ATTRIBUTE, METHOD_ATTRIBUTE, BODY_ATTRIBUTE]
            .into_iter()
            .find(|attribute| !matches!(attributes.get(*attribute), Some(Value::String(_))));
        if let Some(attribute) = missing {
            return Err(PrepareError {
                attempt,
                message: kind.missing_attribute_message(attribute),
            });
        }

        let Value::String(url) = &attributes[URL_ATTRIBUTE] else {
            unreachable!("the required send-phase attributes were checked above")
        };
        let Value::String(method) = &attributes[METHOD_ATTRIBUTE] else {
            unreachable!("the required send-phase attributes were checked above")
        };
        let Value::String(body) = &attributes[BODY_ATTRIBUTE] else {
            unreachable!("the required send-phase attributes were checked above")
        };
        let url = url.clone();
        let method = method.clone();
        let body = body.clone();

        Ok(Self {
            attempt,
            request,
            kind,
            url,
            method,
            body,
        })
    }

    pub fn attributes(&self) -> &BTreeMap<String, Value> {
        &self.request.resource.attributes
    }

    pub fn send(self) -> ProbedCreate {
        let phase = wasi_http_send_phase(&self.url, &self.method, &self.body, self.kind);
        ProbedCreate {
            attempt: self.attempt,
            request: self.request,
            kind: self.kind,
            phase,
        }
    }
}

pub struct ProbedCreate {
    attempt: CreateAttempt,
    request: CreateRequest,
    kind: ProbeKind,
    phase: String,
}

impl ProbedCreate {
    pub fn pending<T: PhasedToken>(
        self,
        token: T::Operation,
        poll_after_secs: u32,
    ) -> CreateStep<T> {
        self.attempt
            .pending::<T>(token, Sent::AllRequestAttributes, poll_after_secs)
    }

    pub fn untracked<T>(self, error: ProviderError) -> CreateVerdict<T> {
        self.attempt.untracked(error)
    }

    pub fn succeeded<T>(self) -> CreateStep<T> {
        let mut attributes = self.request.resource.attributes;
        attributes.insert(SEND_PHASE_ATTRIBUTE.to_string(), Value::String(self.phase));
        let expectation = self.kind.identifier_expectation();
        let identifier = AppliedIdentifier::new(self.kind.identifier()).expect(expectation);
        self.attempt
            .issued(identifier, Sent::AllRequestAttributes)
            .succeeded(attributes)
    }
}

pub fn wasi_http_send_phase(url: &str, method: &str, body: &str, kind: ProbeKind) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        use std::error::Error as _;
        use std::time::Duration;

        use aws_smithy_runtime_api::client::http::{
            HttpClient, HttpConnector, HttpConnectorSettings,
        };
        use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
        use aws_smithy_runtime_api::client::result::SdkError;
        use aws_smithy_runtime_api::client::runtime_components::RuntimeComponentsBuilder;
        use carina_plugin_sdk::wasi_http::{SendPhase, TransportFailure, WasiHttpClient};

        let components = RuntimeComponentsBuilder::for_tests()
            .build()
            .expect(kind.runtime_components_expectation());
        let settings = HttpConnectorSettings::builder()
            .connect_timeout(Duration::from_secs(1))
            // Keep the first-byte timeout short for the silent-server
            // host/guest contract test.
            .read_timeout(Duration::from_secs(1))
            .build();
        let connector = WasiHttpClient::new().http_connector(&settings, &components);
        let mut request = HttpRequest::get(url).expect(kind.url_expectation());
        request.set_method(method).expect(kind.method_expectation());
        *request.body_mut() = body.as_bytes().to_vec().into();

        match futures::executor::block_on(connector.call(request)) {
            Ok(response) if response.status().is_success() => "success".to_string(),
            Ok(response) => format!("status:{}", response.status().as_u16()),
            Err(error) => {
                let sdk_error: SdkError<(), ()> = SdkError::dispatch_failure(error);
                let SdkError::DispatchFailure(dispatch) = &sdk_error else {
                    unreachable!("the probe wraps a connector error as a dispatch failure")
                };
                let connector_error = dispatch
                    .as_connector_error()
                    .expect("dispatch failure must retain its connector error");
                let Some(source) = connector_error.source() else {
                    return "missing_transport_failure_source".to_string();
                };
                let Some(failure) = source.downcast_ref::<TransportFailure>() else {
                    return format!("unexpected_transport_failure_source:{source}");
                };

                let phase = match failure.phase() {
                    SendPhase::NotSent => {
                        if kind.checks_connector_error_kind() {
                            assert!(connector_error.is_io());
                        }
                        "not_sent"
                    }
                    SendPhase::MaybeSent => {
                        if kind.checks_connector_error_kind() {
                            assert!(!connector_error.is_timeout());
                            assert!(!connector_error.is_io());
                            assert!(connector_error.is_other());
                        }
                        "maybe_sent"
                    }
                };
                let code = failure.code().map_or_else(
                    || "none".to_string(),
                    |code| {
                        let debug = format!("{code:?}");
                        debug
                            .strip_prefix("ErrorCode::")
                            .unwrap_or(&debug)
                            .split('(')
                            .next()
                            .expect("an error-code debug name is non-empty")
                            .to_string()
                    },
                );
                format!("{phase}:{code}")
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = (url, method, body, kind);
        "unsupported".to_string()
    }
}
