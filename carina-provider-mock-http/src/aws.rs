#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::BTreeMap;

use carina_plugin_sdk::core::schema::{
    AttributeSchema, AttributeType, ReferenceForms, ResourceSchema, ValidatedConfigSchema,
    ValidatedResourceSchema,
};
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending,
    PendingDelete, PlanOp, Poll, PollVerdict, Resume, ResumeStep, ResumeVerdict, UpdateOutcome,
};

use carina_provider_mock_http::send_phase::{self, PreparedCreate, ProbeKind};

const ACCESS_KEY_ID_ATTRIBUTE: &str = "access_key_id";
const HAS_SESSION_TOKEN_ATTRIBUTE: &str = "has_session_token";

#[derive(Default)]
struct MockHttpAwsProvider;

impl MockHttpAwsProvider {
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
            provider_name: Some("aws".to_string()),
            operation: Some("current_credentials".to_string()),
            status: None,
            code: None,
            request_id: None,
        }
    }
}

impl CarinaProvider for MockHttpAwsProvider {
    type CreateToken = Never;
    type UpdateToken = Never;
    type DeleteToken = Never;

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "aws".to_string(),
            display_name: "Mock HTTP AWS Credentials Provider".to_string(),
            capabilities: Vec::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![
            ResourceSchema::new("test.aws_credentials")
                .attribute(
                    AttributeSchema::provider_populated(
                        ACCESS_KEY_ID_ATTRIBUTE,
                        AttributeType::string(),
                    )
                    .build(),
                )
                .attribute(
                    AttributeSchema::provider_populated(
                        HAS_SESSION_TOKEN_ATTRIBUTE,
                        AttributeType::bool(),
                    )
                    .build(),
                )
                .with_description("Host-resolved AWS credential import probe")
                .validate()
                .expect("mock AWS credential probe schema must be valid"),
            send_phase::schema(ProbeKind::AwsWorld, None),
        ]
    }

    fn reference_forms(&self) -> ReferenceForms {
        BTreeMap::new()
    }

    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::validate(BTreeMap::new())
            .expect("empty mock AWS provider config schema must be valid")
    }

    fn validate_config(&self, _attrs: &BTreeMap<String, Value>) -> Result<(), String> {
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
        let credentials = carina_plugin_sdk::aws_credentials::current().map_err(|error| {
            let kind = match error {
                carina_plugin_sdk::aws_credentials::AwsCredentialError::NotFound(_) => {
                    ProviderErrorKind::InvalidInput
                }
                carina_plugin_sdk::aws_credentials::AwsCredentialError::ProviderFailed(_) => {
                    ProviderErrorKind::ApiError
                }
            };
            Self::error(&resource.id, kind, error.to_string())
        })?;
        let attributes = BTreeMap::from([
            (
                ACCESS_KEY_ID_ATTRIBUTE.to_string(),
                Value::String(credentials.access_key_id().to_string()),
            ),
            (
                HAS_SESSION_TOKEN_ATTRIBUTE.to_string(),
                Value::Bool(credentials.session_token().is_some()),
            ),
        ]);
        Ok(State {
            id: resource.id.clone(),
            identifier: None,
            attributes,
            exists: true,
        })
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> Result<CreateStep<Self::CreateToken>, CreateVerdict<Self::CreateToken>> {
        let probe =
            PreparedCreate::prepare(request, attempt, ProbeKind::AwsWorld).map_err(|error| {
                error.untracked(|message| Self::error(id, ProviderErrorKind::InvalidInput, message))
            })?;
        Ok(probe.send().succeeded())
    }

    fn poll_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        poll: Poll<Self::CreateToken>,
        _poll_request: carina_plugin_sdk::PollRequest,
    ) -> Result<CreateStep<Self::CreateToken>, PollVerdict<Self::CreateToken>> {
        match poll {
            Poll::Unissued(_, token) => match token {},
            Poll::Issued(_, token) => match token {},
        }
    }

    fn resume_create(
        &self,
        _id: &ResourceId,
        _request: CreateRequest,
        resume: Resume<Self::CreateToken>,
        _poll_request: carina_plugin_sdk::PollRequest,
    ) -> Result<ResumeStep<Self::CreateToken>, ResumeVerdict<Self::CreateToken>> {
        match resume {
            Resume::Unissued(_, token) => match token {},
            Resume::Issued(_, token) => match token {},
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
            "the AWS credential probe does not implement update",
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
    MockHttpAwsProvider,
    aws,
    egress: [
        "phase-test.invalid",
        "resolved-metadata.invalid",
    ],
);

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("carina-provider-mock-http-aws is a wasm32-wasip2 test fixture");
}
