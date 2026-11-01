#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::BTreeMap;

use carina_plugin_sdk::core::schema::{
    ReferenceForms, ValidatedConfigSchema, ValidatedResourceSchema,
};
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending,
    PendingDelete, PlanOp, Poll, PollVerdict, Resume, ResumeStep, ResumeVerdict, UpdateOutcome,
};

use carina_provider_mock_http::send_phase::{self, PreparedCreate, ProbeKind};

#[derive(Default)]
struct MockHttpAwsUnusedProvider;

impl MockHttpAwsUnusedProvider {
    fn error(id: &ResourceId, message: impl Into<String>) -> ProviderError {
        ProviderError {
            kind: ProviderErrorKind::InvalidInput,
            message: message.into(),
            resource_id: Some(id.clone()),
            cause: None,
            provider_name: Some("mock_http_aws_unused".to_string()),
            operation: None,
            status: None,
            code: None,
            request_id: None,
        }
    }
}

impl CarinaProvider for MockHttpAwsUnusedProvider {
    type CreateToken = Never;
    type UpdateToken = Never;
    type DeleteToken = Never;

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "mock_http_aws_unused".to_string(),
            display_name: "Mock HTTP AWS Unused Credentials Provider".to_string(),
            capabilities: Vec::new(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![send_phase::schema(ProbeKind::AwsWorld, None)]
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
        Err(Self::error(
            &resource.id,
            "the unused-credentials fixture does not implement data sources",
        ))
    }

    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> Result<CreateStep<Self::CreateToken>, CreateVerdict<Self::CreateToken>> {
        let probe = PreparedCreate::prepare(request, attempt, ProbeKind::AwsWorld)
            .map_err(|error| error.untracked(|message| Self::error(id, message)))?;
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
            Poll::Unissued(_, token) => match *token.as_ref() {},
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
            Resume::Unissued(_, token) => match *token.as_ref() {},
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
            "the unused-credentials fixture does not implement update",
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
carina_plugin_sdk::export_provider!(MockHttpAwsUnusedProvider, aws, egress: [],);

#[cfg(target_arch = "wasm32")]
fn main() {}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("carina-provider-mock-http-aws-unused is a wasm32-wasip2 test fixture");
}
