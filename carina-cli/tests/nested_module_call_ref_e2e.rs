//! End-to-end validation regression for carina#3243.
//!
//! When a module declares `let X = inner_module { ... }` and a sibling
//! resource in the same module references `X.<attr>`, expanding the
//! outer module as a nested call must instance-prefix the reference
//! along with the storage row. Pre-fix, the reference was left bare
//! and `carina validate` failed with `unknown binding 'X' in reference
//! X.<attr>`.
//!
//! This test drives the full `carina-cli` validate surface against a
//! three-directory multi-file fixture (caller / outer / inner) — the
//! same shape as the real `carina-rs/infra` bootstrap stack the issue
//! was filed against.

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;
use std::collections::HashMap;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Minimal awscc provider factory exposing the two resource types the
// fixture needs: `ec2.Vpc` and `ec2.SecurityGroup`.
// ---------------------------------------------------------------------------

struct AwsccTestFactory;

impl ProviderFactory for AwsccTestFactory {
    fn name(&self) -> &str {
        "awscc"
    }
    fn display_name(&self) -> &str {
        "AWSCC (carina#3243 test stub)"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        _type_name: &carina_core::schema::TypeIdentity,
        _value: &carina_core::provider::CanonicalCustomTypeValue,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }
    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }
    fn create_provider(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(NoopProvider)))
        })
    }
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _config: &carina_core::provider::ProviderReadyConfig,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> =
            vec![vpc_schema(), security_group_schema()];
        schema_drafts
            .into_iter()
            .map(|schema| {
                schema
                    .validate()
                    .expect("factory test schema must be valid")
            })
            .collect()
    }
}

fn vpc_schema() -> ResourceSchema {
    ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", AttributeType::string()).build())
}

fn security_group_schema() -> ResourceSchema {
    ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::optional("group_description", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", AttributeType::string()).build())
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "awscc"
    }
    fn read(
        &self,
        id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ReadOutcome>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::provider::ReadOutcome::not_found(id)) })
    }
    fn read_data_source(
        &self,
        resource: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn create(
        &self,
        _id: &carina_core::resource::ResourceId,
        _request: carina_core::provider::CreateRequest,
        attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        Box::pin(async move {
            Ok(attempt
                .issued(
                    carina_core::provider::AppliedIdentifier::new("mock-id")
                        .expect("fixture identifier is non-empty"),
                    carina_core::provider::Sent::AllRequestAttributes,
                )
                .succeeded(HashMap::new()))
        })
    }
    fn resume_create(
        &self,
        _request: carina_core::provider::ResumeCreateRequest,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async { panic!("validation fixture never resumes a create") })
    }
    fn update(
        &self,
        id: &carina_core::resource::ResourceId,
        identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        let identifier = identifier.to_string();
        Box::pin(async move {
            Ok(carina_core::provider::UpdateOutcome::Success {
                state: carina_core::provider::AppliedState::new(id, identifier, HashMap::new())
                    .expect("provider input identifier is non-empty"),
            })
        })
    }
    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }

    fn required_permissions(
        &self,
        _id: &carina_core::resource::ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

fn factories() -> Vec<carina_cli::wiring::LoadedProviderFactory> {
    carina_cli::wiring::LoadedProviderFactory::for_test(vec![
        Box::new(AwsccTestFactory) as Box<dyn ProviderFactory>
    ])
}

/// Build a three-directory fixture:
///   <tempdir>/inner/main.crn       — leaf module
///   <tempdir>/outer/main.crn       — middle module that calls `inner`
///   <tempdir>/caller/main.crn      — root that calls `outer`
/// and return the tempdir + caller path.
fn write_fixture() -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");

    std::fs::create_dir(dir.path().join("inner")).unwrap();
    std::fs::create_dir(dir.path().join("outer")).unwrap();
    std::fs::create_dir(dir.path().join("caller")).unwrap();

    std::fs::write(
        dir.path().join("inner/main.crn"),
        r#"arguments {
  cidr_block: String
}

attributes {
  vpc_id = vpc.vpc_id
}

let vpc = awscc.ec2.Vpc {
  cidr_block = cidr_block
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("outer/main.crn"),
        r#"arguments {
  cidr_block: String = '10.0.0.0/16'
}

let inner = use { source = '../inner' }

let net = inner {
  cidr_block = cidr_block
}

let sg = awscc.ec2.SecurityGroup {
  group_description = 'test'
  vpc_id            = net.vpc_id
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("caller/providers.crn"),
        r#"provider awscc {
  region = "us-east-1"
}
"#,
    )
    .unwrap();

    std::fs::write(
        dir.path().join("caller/main.crn"),
        r#"let outer = use { source = '../outer' }

let o = outer {
  cidr_block = '10.1.0.0/16'
}
"#,
    )
    .unwrap();

    dir
}

#[test]
fn validate_resolves_intra_module_ref_to_module_call_binding() {
    let fixture = write_fixture();
    let caller = fixture.path().join("caller");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        !diags.iter().any(|d| d.contains("unknown binding")),
        "validate must not flag an intra-module reference to a module-call binding \
         as `unknown binding`; got: {:#?}",
        diags
    );
    assert!(
        diags.is_empty(),
        "validate must produce no errors for the well-formed fixture; got: {:#?}",
        diags
    );
}
