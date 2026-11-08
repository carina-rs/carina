#![forbid(unsafe_code)]

use carina_plugin_sdk::core::schema::{ValidatedConfigSchema, ValidatedResourceSchema};
use carina_plugin_sdk::types::*;
use carina_plugin_sdk::{
    CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending,
    PendingDelete, PlanOp, Poll, PollRequest, PollVerdict, Resume, ResumeStep, ResumeVerdict,
    UpdateOutcome,
};
use std::collections::BTreeMap;

#[derive(Default)]
struct SafeProvider;

impl CarinaProvider for SafeProvider {
    type CreateToken = Never;
    type UpdateToken = Never;
    type DeleteToken = Never;

    fn info(&self) -> ProviderInfo {
        ProviderInfo {
            name: "safe-provider".into(),
            display_name: "Safe provider".into(),
            capabilities: Vec::new(),
            version: "0.0.0".into(),
        }
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        Vec::new()
    }
    fn reference_forms(&self) -> carina_plugin_sdk::core::schema::ReferenceForms {
        BTreeMap::new()
    }
    fn assignability_edges(&self) -> Vec<carina_plugin_sdk::core::schema::AssignabilityEdge> {
        Vec::new()
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        ValidatedConfigSchema::default()
    }
    fn validate_config(&self, _: &BTreeMap<String, Value>) -> Result<(), ProviderError> {
        Ok(())
    }
    fn initialize(&mut self, _: &BTreeMap<String, Value>) -> Result<ReferenceScope, ProviderError> {
        Ok(ReferenceScope::Unavailable)
    }
    fn read(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: ReadRequest,
    ) -> Result<ReadOutcome, ProviderError> {
        unreachable!()
    }
    fn read_data_source(&self, _: &Resource) -> Result<State, ProviderError> {
        unreachable!()
    }
    fn create(
        &self,
        _: &ResourceId,
        _: CreateRequest,
        _: CreateAttempt,
    ) -> Result<CreateStep<Never>, CreateVerdict<Never>> {
        unreachable!()
    }
    fn poll_create(
        &self,
        _: &ResourceId,
        _: CreateRequest,
        _: Poll<Never>,
        _: PollRequest,
    ) -> Result<CreateStep<Never>, PollVerdict<Never>> {
        unreachable!()
    }
    fn resume_create(
        &self,
        _: &ResourceId,
        _: CreateRequest,
        _: Resume<Never>,
        _: PollRequest,
    ) -> Result<ResumeStep<Never>, ResumeVerdict<Never>> {
        unreachable!()
    }
    fn update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
    ) -> Result<UpdateOutcome<Never>, ProviderError> {
        unreachable!()
    }
    fn poll_update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
        _: Pending<Never>,
        _: PollRequest,
    ) -> Result<UpdateOutcome<Never>, ProviderError> {
        unreachable!()
    }
    fn delete(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: DeleteRequest,
    ) -> Result<DeleteOutcome<Never>, ProviderError> {
        unreachable!()
    }
    fn poll_delete(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: DeleteRequest,
        _: PendingDelete<Never>,
    ) -> Result<DeleteOutcome<Never>, ProviderError> {
        unreachable!()
    }
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: PlanOp,
    ) -> Result<Vec<String>, ProviderError> {
        Ok(Vec::new())
    }
}

#[cfg(not(any(feature = "export-http", feature = "export-aws")))]
carina_plugin_sdk::export_provider!(SafeProvider);
#[cfg(feature = "export-http")]
carina_plugin_sdk::export_provider!(SafeProvider, http, egress: ["api.example.com"]);
#[cfg(feature = "export-aws")]
carina_plugin_sdk::export_provider!(SafeProvider, aws, egress: ["api.example.com"]);
