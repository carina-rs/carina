//! Constructor-time panic regression fixture for the SDK export bridge.
#[cfg(target_arch = "wasm32")]
mod fixture {
    use carina_plugin_sdk::types::*;
    use carina_plugin_sdk::{
        CarinaProvider, CreateAttempt, CreateStep, CreateVerdict, DeleteOutcome, Never, Pending,
        PendingDelete, PlanOp, Poll, PollRequest, PollVerdict, ProviderDiagnostics, Resume,
        ResumeStep, ResumeVerdict, UpdateOutcome,
    };
    use std::collections::BTreeMap;
    struct ConstructorPanic;
    impl Default for ConstructorPanic {
        fn default() -> Self {
            ProviderDiagnostics::guest()
                .register_github_token(
                    carina_plugin_sdk::protected_material::GithubTokenMaterial::new(
                        "constructor-secret-canary".into(),
                    ),
                )
                .expect("constructor credential registration must succeed");
            panic!("constructor keeps context constructor-secret-canary");
        }
    }
    impl CarinaProvider for ConstructorPanic {
        type CreateToken = Never;
        type UpdateToken = Never;
        type DeleteToken = Never;
        fn info(&self) -> ProviderInfo {
            ProviderInfo {
                name: "mock".into(),
                display_name: "constructor probe".into(),
                version: "0.0.0".into(),
                capabilities: vec![],
            }
        }
        fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
            vec![]
        }
        fn reference_forms(&self) -> carina_core::schema::ReferenceForms {
            BTreeMap::new()
        }
        fn assignability_edges(&self) -> Vec<carina_core::schema::AssignabilityEdge> {
            vec![]
        }
        fn validate_config(&self, _attrs: &BTreeMap<String, Value>) -> Result<(), ProviderError> {
            Ok(())
        }
        fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
            carina_core::schema::ValidatedConfigSchema::validate(BTreeMap::new()).unwrap()
        }
        fn initialize(
            &mut self,
            _attrs: &BTreeMap<String, Value>,
        ) -> Result<ReferenceScope, ProviderError> {
            Ok(ReferenceScope::Unavailable)
        }
        fn read(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: ReadRequest,
        ) -> Result<ReadOutcome, ProviderError> {
            unreachable!()
        }
        fn read_data_source(&self, _resource: &Resource) -> Result<State, ProviderError> {
            unreachable!()
        }
        fn create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            _attempt: CreateAttempt,
        ) -> Result<CreateStep<Never>, CreateVerdict<Never>> {
            unreachable!()
        }
        fn poll_create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            _poll: Poll<Never>,
            _request_poll: PollRequest,
        ) -> Result<CreateStep<Never>, PollVerdict<Never>> {
            unreachable!()
        }
        fn resume_create(
            &self,
            _id: &ResourceId,
            _request: CreateRequest,
            _resume: Resume<Never>,
            _poll: PollRequest,
        ) -> Result<ResumeStep<Never>, ResumeVerdict<Never>> {
            unreachable!()
        }
        fn update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
        ) -> Result<UpdateOutcome<Never>, ProviderError> {
            unreachable!()
        }
        fn poll_update(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: UpdateRequest,
            _pending: Pending<Never>,
            _poll: PollRequest,
        ) -> Result<UpdateOutcome<Never>, ProviderError> {
            unreachable!()
        }
        fn delete(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: DeleteRequest,
        ) -> Result<DeleteOutcome<Never>, ProviderError> {
            unreachable!()
        }
        fn poll_delete(
            &self,
            _id: &ResourceId,
            _identifier: &AppliedIdentifier,
            _request: DeleteRequest,
            _pending: PendingDelete<Never>,
        ) -> Result<DeleteOutcome<Never>, ProviderError> {
            unreachable!()
        }
        fn required_permissions(
            &self,
            _id: &ResourceId,
            _op: PlanOp,
        ) -> Result<Vec<String>, ProviderError> {
            Ok(vec![])
        }
    }
    carina_plugin_sdk::export_provider!(ConstructorPanic);
}
fn main() {}
