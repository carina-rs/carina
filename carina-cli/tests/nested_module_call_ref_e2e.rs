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
use carina_core::resource::{DataSource, Value};
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
    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }
    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        _type_name: &carina_core::schema::TypeIdentity,
        _value: &str,
    ) -> Result<(), String> {
        Ok(())
    }
    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "us-east-1".to_string()
    }
    fn create_provider(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(NoopProvider) as Box<dyn Provider>) })
    }
    fn create_normalizer(
        &self,
        _binding: Option<&str>,
        _attributes: &IndexMap<String, Value>,
    ) -> BoxFuture<'_, Box<dyn ProviderNormalizer>> {
        Box::pin(async { Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer> })
    }
    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![vpc_schema(), security_group_schema()]
    }
}

fn vpc_schema() -> ResourceSchema {
    ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("cidr_block", AttributeType::String))
        .attribute(AttributeSchema::new("vpc_id", AttributeType::String))
}

fn security_group_schema() -> ResourceSchema {
    ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::new(
            "group_description",
            AttributeType::String,
        ))
        .attribute(AttributeSchema::new("vpc_id", AttributeType::String))
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "awscc"
    }
    fn read(
        &self,
        id: &carina_core::resource::ResourceId,
        _identifier: Option<&str>,
        _request: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::not_found(id)) })
    }
    fn read_data_source(
        &self,
        resource: &DataSource,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = resource.id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn create(
        &self,
        id: &carina_core::resource::ResourceId,
        _request: carina_core::provider::CreateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn update(
        &self,
        id: &carina_core::resource::ResourceId,
        _identifier: &str,
        _request: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::resource::State>> {
        let id = id.clone();
        Box::pin(async move { Ok(carina_core::resource::State::existing(id, HashMap::new())) })
    }
    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &str,
        _request: carina_core::provider::DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move { Ok(()) })
    }
}

fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(AwsccTestFactory) as Box<dyn ProviderFactory>]
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
