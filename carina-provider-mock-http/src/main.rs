#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::BTreeMap;

use carina_plugin_sdk::core::schema::{
    AttributeType as CoreAttributeType, ValidatedConfigSchema as CoreValidatedConfigSchema,
    ValidatedResourceSchema as CoreValidatedResourceSchema,
};
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending,
    PendingDelete, PlanOp, Poll, PollVerdict, Resume, ResumeStep, ResumeVerdict, UpdateOutcome,
};

use carina_provider_mock_http::send_phase::{self, PreparedCreate, ProbeKind};

const CREATE_BEHAVIOR_ATTRIBUTE: &str = "create_behavior";
const VALIDATE_EGRESS_URL_ATTRIBUTE: &str = "validate_egress_url";

#[derive(Default)]
struct MockHttpProvider;

#[derive(Debug, serde::Deserialize)]
struct CreateToken(f64);

impl serde::Serialize for CreateToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if !self.0.is_finite() {
            return Err(serde::ser::Error::custom(
                "intentional non-finite mock HTTP create token",
            ));
        }
        serde::Serialize::serialize(&self.0, serializer)
    }
}

impl MockHttpProvider {
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
}

impl CarinaProvider for MockHttpProvider {
    type CreateToken = CreateToken;
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

    fn schemas(&self) -> Vec<CoreValidatedResourceSchema> {
        vec![send_phase::schema(
            ProbeKind::HttpWorld,
            Some(CREATE_BEHAVIOR_ATTRIBUTE),
        )]
    }

    fn reference_forms(&self) -> carina_plugin_sdk::core::schema::ReferenceForms {
        BTreeMap::new()
    }

    fn provider_config_attribute_types(&self) -> CoreValidatedConfigSchema {
        CoreValidatedConfigSchema::validate(BTreeMap::from([(
            VALIDATE_EGRESS_URL_ATTRIBUTE.to_string(),
            CoreAttributeType::string(),
        )]))
        .expect("mock HTTP provider config schema must be valid")
    }

    fn validate_config(&self, attrs: &BTreeMap<String, Value>) -> Result<(), String> {
        if let Some(Value::String(url)) = attrs.get(VALIDATE_EGRESS_URL_ATTRIBUTE) {
            let _outcome = send_phase::wasi_http_send_phase(url, "GET", "", ProbeKind::HttpWorld);
        }
        Ok(())
    }

    fn initialize(&mut self, _attrs: &BTreeMap<String, Value>) -> Result<ReferenceScope, String> {
        Ok(ReferenceScope::Unavailable)
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
    ) -> Result<CreateStep<Self::CreateToken>, CreateVerdict<Self::CreateToken>> {
        let probe =
            PreparedCreate::prepare(request, attempt, ProbeKind::HttpWorld).map_err(|error| {
                error.untracked(|message| Self::error(id, ProviderErrorKind::InvalidInput, message))
            })?;
        let behavior = probe
            .attributes()
            .get(CREATE_BEHAVIOR_ATTRIBUTE)
            .and_then(|value| match value {
                Value::String(value) => Some(value.clone()),
                Value::Bool(_value) => None,
                Value::Int(_value) => None,
                Value::Float(_value) => None,
                Value::StringList(_value) => None,
                Value::List(_value) => None,
                Value::Map(_value) => None,
            });
        if behavior.as_deref() == Some("trap_before_send") {
            panic!("intentional mock HTTP trap before send");
        }
        let probe = probe.send();
        match behavior.as_deref() {
            Some("trap_after_send") => panic!("intentional mock HTTP trap after send"),
            Some("pending_nan_after_send") => {
                return Ok(probe.pending(CreateToken(f64::NAN), 1));
            }
            Some("untracked_after_send") => {
                return Err(probe.untracked(Self::error(
                    id,
                    ProviderErrorKind::ApiError,
                    "provider declared untracked after send",
                )));
            }
            None | Some("normal") => {}
            Some(unsupported) => {
                return Err(probe.untracked(Self::error(
                    id,
                    ProviderErrorKind::InvalidInput,
                    format!("unsupported create behavior {unsupported}"),
                )));
            }
        }
        Ok(probe.succeeded())
    }

    fn poll_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        _poll: Poll<Self::CreateToken>,
        _poll_request: carina_plugin_sdk::PollRequest,
    ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>> {
        unreachable!("the NaN fixture token cannot cross the guest boundary")
    }

    fn resume_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        _resume: Resume<Self::CreateToken>,
        _poll_request: carina_plugin_sdk::PollRequest,
    ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict<Self::CreateToken>> {
        unreachable!("the NaN fixture token cannot cross the guest boundary")
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
        _poll_request: carina_plugin_sdk::PollRequest,
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
        pending: PendingDelete<Self::DeleteToken>,
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
carina_plugin_sdk::export_provider!(
    MockHttpProvider,
    http,
    egress: [
        "phase-test.invalid",
        "resolved-metadata.invalid",
    ],
);

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("carina-provider-mock-http is a wasm32-wasip2 test fixture");
}
