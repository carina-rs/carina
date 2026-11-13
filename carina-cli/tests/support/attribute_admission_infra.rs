//! Shared schema catalog and recording provider for the infrastructure regressions.

use std::sync::{Arc, Mutex};

use carina_core::provider::{
    AppliedIdentifier, CreateAttempt, CreateFailure, CreateOutcome, CreateRequest, Provider,
    ProviderReadyDataSource, ReadOutcome, ReadRequest, ResumeCreateRequest, ResumeFailure,
    ResumeOutcome, Sent, UpdateOutcome, UpdateRequest,
};
use carina_core::provider::{BoxFuture, ProviderFactory, ProviderInitialization, ProviderResult};
use carina_core::resource::{ConcreteValue, ProviderResource, ResourceId, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, PatternMatch, PatternSpec, ResourceSchema, StructField,
    TypeError, TypeIdentity, ValidatedResourceSchema,
};

pub struct InfraFactory(pub &'static str);

impl ProviderFactory for InfraFactory {
    fn name(&self) -> &str {
        self.0
    }
    fn display_name(&self) -> &str {
        "Admission infrastructure fixture"
    }

    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async {
            unreachable!(
                "these fixtures use the planner and preparation gate without cloud dispatch"
            )
        })
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        let group = |max| {
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("aws.identitystore.Group.Id")),
                None,
                Some((Some(1), Some(max))),
                None,
            )
        };
        let arn = |length| {
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("aws.iam.Role.Arn")),
                None,
                length,
                None,
            )
        };
        let drafts = match self.0 {
            "aws" => vec![
                ResourceSchema::new("identitystore.Group").attribute(AttributeSchema::provider_populated("group_id", group(64)).build()),
                ResourceSchema::new("iam.Role").attribute(AttributeSchema::provider_populated("arn", arn(None)).build()),
            ],
            "awscc" => vec![
                ResourceSchema::new("sso.Assignment")
                    .attribute(AttributeSchema::optional("account_id", AttributeType::string()).create_only().identity().build())
                    .attribute(AttributeSchema::required("principal_id", AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("aws.identitystore.Group.Id")),
                        Some(PatternSpec::new("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$", PatternMatch::Full)),
                        Some((Some(1), Some(36))), None,
                    )).build()),
                ResourceSchema::new("scheduler.Schedule").attribute(AttributeSchema::required("target", AttributeType::struct_("Target", vec![
                    StructField::required("role_arn", arn(Some((Some(1), Some(1600))))).build(),
                ])).build()),
            ],
            owner => panic!("unexpected fixture provider {owner}"),
        };
        drafts
            .into_iter()
            .map(|schema| schema.validate().unwrap())
            .collect()
    }
}

/// The source publishes its output only after create crosses the provider gate.
pub struct OutputProvider {
    pub name: &'static str,
    pub output: String,
    pub creates: Arc<Mutex<Vec<ProviderResource>>>,
}

impl Provider for OutputProvider {
    fn name(&self) -> &str {
        self.name
    }

    fn read(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<ReadOutcome>> {
        Box::pin(async { unreachable!("create-only fixture") })
    }
    fn read_data_source(
        &self,
        _: &ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<State>> {
        Box::pin(async { unreachable!("fixture has no data sources") })
    }
    fn create(
        &self,
        id: &ResourceId,
        request: CreateRequest,
        attempt: CreateAttempt,
    ) -> BoxFuture<'_, Result<CreateOutcome, CreateFailure>> {
        let resource = request.resource().as_resource();
        self.creates.lock().unwrap().push(resource.clone());
        let mut attributes = resource.resolved_attributes();
        let output_attribute = match id.resource_type.as_str() {
            "identitystore.Group" => Some("group_id"),
            "iam.Role" => Some("arn"),
            _ => None,
        };
        if let Some(name) = output_attribute {
            attributes.insert(
                name.into(),
                Value::Concrete(ConcreteValue::String(self.output.clone().into())),
            );
        }
        Box::pin(async move {
            Ok(attempt
                .issued(
                    AppliedIdentifier::new("fixture-id").unwrap(),
                    Sent::AllRequestAttributes,
                )
                .succeeded(attributes))
        })
    }
    fn resume_create(
        &self,
        _: ResumeCreateRequest,
    ) -> BoxFuture<'_, Result<ResumeOutcome, ResumeFailure>> {
        Box::pin(async { unreachable!("fixture never resumes") })
    }
    fn update(
        &self,
        _: &ResourceId,
        _: &AppliedIdentifier,
        _: UpdateRequest,
        _: carina_core::provider::UpdateAttempt,
    ) -> BoxFuture<'_, ProviderResult<UpdateOutcome>> {
        Box::pin(async { unreachable!("create-only fixture") })
    }
    fn delete(&self, _: carina_core::provider::DeleteAttempt) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { unreachable!("create-only fixture") })
    }
    fn required_permissions(
        &self,
        _: &ResourceId,
        _: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

pub struct RuntimeInfraFactory {
    pub name: &'static str,
    pub output: String,
    pub creates: Arc<Mutex<Vec<ProviderResource>>>,
}

impl ProviderFactory for RuntimeInfraFactory {
    fn name(&self) -> &str {
        self.name
    }
    fn display_name(&self) -> &str {
        "Recording admission fixture"
    }

    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        InfraFactory(self.name).schemas()
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        InfraFactory(self.name).schema_witness()
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async move {
            Ok(
                carina_core::provider::InitializeOutcome::unavailable().with_provider(Box::new(
                    OutputProvider {
                        name: self.name,
                        output: self.output.clone(),
                        creates: self.creates.clone(),
                    },
                )),
            )
        })
    }
}

pub fn runtime_factories(
    output: &str,
    creates: &Arc<Mutex<Vec<ProviderResource>>>,
) -> Vec<Box<dyn ProviderFactory>> {
    ["aws", "awscc"]
        .into_iter()
        .map(|name| {
            Box::new(RuntimeInfraFactory {
                name,
                output: output.into(),
                creates: creates.clone(),
            }) as Box<dyn ProviderFactory>
        })
        .collect()
}

/// The exact diagnostics for the fixture outputs, independent of validation.
pub fn expected_literal_errors(attribute: &str, output: &str) -> Vec<TypeError> {
    match (attribute, output) {
        ("principal_id", "12345678-1234-1234-1234-123456789abc")
        | ("target", "arn:aws:iam::123456789012:role/scheduler") => Vec::new(),
        ("principal_id", value) if value == "invalid-uuid" || value == "a".repeat(64) => {
            vec![TypeError::PatternMismatch {
                value: value.into(),
                pattern: PatternSpec::new(
                    "^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
                    PatternMatch::Full,
                )
                .into(),
                attribute: Some("principal_id".into()),
                type_name: Some("Id".into()),
            }]
        }
        ("target", value) if value.is_empty() || value == "x".repeat(1601) => {
            vec![TypeError::StructFieldError {
                field: "role_arn".into(),
                inner: Box::new(TypeError::LengthOutOfRange {
                    value: value.into(),
                    length: value.len(),
                    min: Some(1),
                    max: Some(1600),
                    attribute: Some("target".into()),
                    type_name: Some("Arn".into()),
                }),
            }]
        }
        _ => panic!("unexpected fixture output for {attribute}: {output:?}"),
    }
}
