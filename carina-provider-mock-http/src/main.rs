#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::HashMap;

use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending,
    PlanOp, Poll, PollVerdict, Resume, ResumeStep, ResumeVerdict, Sent, UpdateOutcome,
};

const URL_ATTRIBUTE: &str = "url";
const METHOD_ATTRIBUTE: &str = "method";
const BODY_ATTRIBUTE: &str = "body";
const SEND_PHASE_ATTRIBUTE: &str = "send_phase";

#[derive(Default)]
struct MockHttpProvider;

impl MockHttpProvider {
    fn string_attribute(name: &str, required: bool, read_only: bool) -> AttributeSchema {
        AttributeSchema {
            name: name.to_string(),
            attr_type: AttributeType::String {
                pattern: None,
                length: None,
                validate: None,
                to_dsl: None,
                identity: None,
            },
            required,
            default: None,
            description: None,
            create_only: false,
            read_only,
            write_only: false,
            block_name: None,
            provider_name: None,
            removable: None,
            identity: false,
            deferred_populate: false,
        }
    }

    fn error(
        id: &ResourceId,
        kind: ProviderErrorKind,
        message: impl Into<String>,
    ) -> ProviderError {
        ProviderError {
            kind,
            message: message.into(),
            resource_id: Some(id.clone()),
            cause: None,
            provider_name: Some("mock_http".to_string()),
            operation: None,
            status: None,
            code: None,
            request_id: None,
        }
    }

    fn wasi_http_send_phase(url: &str, method: &str, body: &str) -> String {
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
                .expect("HTTP probe runtime components must be valid");
            let settings = HttpConnectorSettings::builder()
                .connect_timeout(Duration::from_secs(1))
                // Keep the first-byte timeout short for the silent-server
                // host/guest contract test.
                .read_timeout(Duration::from_secs(1))
                .build();
            let connector = WasiHttpClient::new().http_connector(&settings, &components);
            let mut request = HttpRequest::get(url).expect("HTTP probe URL must be valid");
            request
                .set_method(method)
                .expect("HTTP probe method must be valid");
            *request.body_mut() = body.as_bytes().to_vec().into();

            return match futures::executor::block_on(connector.call(request)) {
                Ok(_) => "success".to_string(),
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
                            assert!(connector_error.is_io());
                            "not_sent"
                        }
                        SendPhase::MaybeSent => {
                            assert!(!connector_error.is_timeout());
                            assert!(!connector_error.is_io());
                            assert!(connector_error.is_other());
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
            };
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (url, method, body);
            "unsupported".to_string()
        }
    }
}

impl CarinaProvider for MockHttpProvider {
    type CreateToken = Never;
    type UpdateToken = Never;
    type DeleteToken = Never;

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "mock_http".to_string(),
            display_name: "Mock HTTP Provider".to_string(),
            capabilities: Vec::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![ResourceSchema {
            resource_type: "test.send_phase".to_string(),
            attributes: HashMap::from([
                (
                    URL_ATTRIBUTE.to_string(),
                    Self::string_attribute(URL_ATTRIBUTE, true, false),
                ),
                (
                    METHOD_ATTRIBUTE.to_string(),
                    Self::string_attribute(METHOD_ATTRIBUTE, true, false),
                ),
                (
                    BODY_ATTRIBUTE.to_string(),
                    Self::string_attribute(BODY_ATTRIBUTE, true, false),
                ),
                (
                    SEND_PHASE_ATTRIBUTE.to_string(),
                    Self::string_attribute(SEND_PHASE_ATTRIBUTE, false, true),
                ),
            ]),
            description: Some("WASI HTTP send-phase probe".to_string()),
            kind: SchemaKind::Managed,
            unique_name: UniqueNameSpec::default(),
            operation_config: None,
            validators: Vec::new(),
            exclusive_required: Vec::new(),
            defs: std::collections::BTreeMap::new(),
        }]
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attrs: &HashMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn read(
        &self,
        id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: ReadRequest,
    ) -> Result<ReadOutcome, ProviderError> {
        Ok(ReadOutcome::NotFound { id: id.clone() })
    }

    fn read_data_source(&self, resource: &Resource) -> Result<State, ProviderError> {
        Err(Self::error(
            &resource.id,
            ProviderErrorKind::InvalidInput,
            "the HTTP probe does not implement data sources",
        ))
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> Result<CreateStep<Self::CreateToken>, CreateVerdict> {
        let Some(Value::String(url)) = request.resource.attributes.get(URL_ATTRIBUTE) else {
            return Err(attempt.untracked(Self::error(
                id,
                ProviderErrorKind::InvalidInput,
                "the HTTP probe requires a string url attribute",
            )));
        };
        let Some(Value::String(method)) = request.resource.attributes.get(METHOD_ATTRIBUTE) else {
            return Err(attempt.untracked(Self::error(
                id,
                ProviderErrorKind::InvalidInput,
                "the HTTP probe requires a string method attribute",
            )));
        };
        let Some(Value::String(body)) = request.resource.attributes.get(BODY_ATTRIBUTE) else {
            return Err(attempt.untracked(Self::error(
                id,
                ProviderErrorKind::InvalidInput,
                "the HTTP probe requires a string body attribute",
            )));
        };
        let phase = Self::wasi_http_send_phase(url, method, body);
        let mut attributes = request.resource.attributes;
        attributes.insert(SEND_PHASE_ATTRIBUTE.to_string(), Value::String(phase));
        let identifier =
            AppliedIdentifier::new("send-phase-probe").expect("the probe identifier is non-empty");

        Ok(attempt
            .issued(identifier, Sent::AllRequestAttributes)
            .succeeded(attributes))
    }

    fn poll_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        poll: Poll<Self::CreateToken>,
    ) -> Result<CreateStep<Self::CreateToken>, PollVerdict> {
        match poll {
            Poll::Unissued(_, token) | Poll::Issued(_, token) => match token {},
        }
    }

    fn resume_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        resume: Resume<Self::CreateToken>,
    ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict> {
        match resume {
            Resume::Unissued(_, token) | Resume::Issued(_, token) => match token {},
        }
    }

    fn update(
        &self,
        id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: UpdateRequest,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
        Err(Self::error(
            id,
            ProviderErrorKind::InvalidInput,
            "the HTTP probe does not implement update",
        ))
    }

    fn poll_update(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: UpdateRequest,
        pending: Pending<Self::UpdateToken>,
    ) -> Result<UpdateOutcome<Self::UpdateToken>, ProviderError> {
        match pending.token {}
    }

    fn delete(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
        Ok(DeleteOutcome::Deleted)
    }

    fn poll_delete(
        &self,
        _id: &ResourceId,
        _identifier: &AppliedIdentifier,
        _request: DeleteRequest,
        pending: Pending<Self::DeleteToken>,
    ) -> Result<DeleteOutcome<Self::DeleteToken>, ProviderError> {
        match pending.token {}
    }

    fn required_permissions(
        &self,
        _id: &ResourceId,
        _op: PlanOp,
    ) -> Result<Vec<String>, ProviderError> {
        Ok(Vec::new())
    }
}

#[cfg(target_arch = "wasm32")]
carina_plugin_sdk::export_provider!(MockHttpProvider, http);

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("carina-provider-mock-http is a wasm32-wasip2 test fixture");
}
