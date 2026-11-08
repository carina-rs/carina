//! Shared schema catalog and recording provider for the infrastructure regressions.

use std::collections::HashMap;
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
    TypeIdentity, ValidatedResourceSchema,
};

pub struct InfraFactory(pub &'static str);

impl ProviderFactory for InfraFactory {
    fn name(&self) -> &str {
        self.0
    }
    fn display_name(&self) -> &str {
        "Admission infrastructure fixture"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
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
            self.provider_config_attribute_types(),
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
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
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

/// Compute the literal-equivalent diagnostics independently of the executor.
pub fn literal_errors(
    schemas: &carina_core::schema::SchemaRegistry,
    catalog: &carina_core::parser::ProviderContext,
    destination: &ResourceId,
    attribute: &str,
    value: Value,
) -> Vec<carina_core::schema::TypeError> {
    let values = HashMap::from([(attribute.to_owned(), value)]);
    let classified = values
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                value
                    .knowledge(&carina_core::resource::knowledge::KnowledgeContext::validation())
                    .unwrap()
                    .into_classification(),
            )
        })
        .collect();
    schemas
        .get(
            &destination.provider,
            &destination.resource_type,
            carina_core::schema::SchemaKind::Resource,
        )
        .unwrap()
        .validate_with_resolver(&classified, &|_| false, catalog.custom_type_resolver())
        .err()
        .unwrap_or_default()
}
