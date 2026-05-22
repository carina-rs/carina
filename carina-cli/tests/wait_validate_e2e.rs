//! End-to-end validation tests for the `wait` construct (carina#2825).
//!
//! Phase 7+8 of the wait construct: assert that `carina validate`
//! (CLI surface) accepts a well-formed multi-file `.crn` directory
//! containing a wait declaration, and surfaces the load-bearing
//! diagnostics for malformed wait blocks (unknown target, unknown
//! attribute, non-`==` operator).
//!
//! Per CLAUDE.md "directory-scoped, never single-file": every fixture
//! is a multi-file `tempfile::tempdir()` layout — wait, target, and
//! provider declarations live in separate files so we exercise the
//! merged-parse path that LSP and CLI share.

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{DataSource, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;
use std::collections::HashMap;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Minimal aws provider factory exposing `aws.acm.Certificate` so the
// validate pipeline sees a real resource type.
// ---------------------------------------------------------------------------

struct WaitTestFactory;

impl ProviderFactory for WaitTestFactory {
    fn name(&self) -> &str {
        "aws"
    }
    fn display_name(&self) -> &str {
        "AWS (wait-construct test stub)"
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
        vec![cert_schema()]
    }
}

fn cert_schema() -> ResourceSchema {
    ResourceSchema::new("acm.Certificate")
        .attribute(AttributeSchema::new("domain_name", AttributeType::String))
        .attribute(AttributeSchema::new(
            "validation_method",
            AttributeType::String,
        ))
        .attribute(AttributeSchema::new("status", AttributeType::String))
        .attribute(AttributeSchema::new("arn", AttributeType::String))
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "aws"
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
    vec![Box::new(WaitTestFactory) as Box<dyn ProviderFactory>]
}

fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).expect("write fixture");
    }
    dir
}

fn cli_validate(fixture: &TempDir) -> Vec<String> {
    carina_cli::commands::validate::validate_with_factories(fixture.path(), factories())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn validate_accepts_well_formed_wait_multi_file_fixture() {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            r#"provider aws {
    region = "us-east-1"
}
"#,
        ),
        (
            "main.crn",
            r#"let cert = aws.acm.Certificate {
    domain_name       = "registry.example.com"
    validation_method = "DNS"
}
"#,
        ),
        (
            "wait.crn",
            r#"let cert_issued = wait cert {
    until   = cert.status == "ISSUED"
    timeout = 75min
}
"#,
        ),
    ]);

    let diags = cli_validate(&fixture);
    assert!(
        !diags.iter().any(|d| d.contains("wait")),
        "well-formed wait fixture must not produce wait-related errors; got: {:?}",
        diags
    );
}

#[test]
fn validate_flags_wait_unknown_target() {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            r#"provider aws {
    region = "us-east-1"
}
"#,
        ),
        (
            "main.crn",
            r#"let cert = aws.acm.Certificate {
    domain_name       = "registry.example.com"
    validation_method = "DNS"
}
"#,
        ),
        (
            "wait.crn",
            r#"let waited = wait nonexistent {
    until = nonexistent.status == "ISSUED"
}
"#,
        ),
    ]);

    let diags = cli_validate(&fixture);
    assert!(
        diags.iter().any(|d| d.contains("nonexistent")),
        "validate must surface unknown wait target; got: {:?}",
        diags
    );
}

#[test]
fn validate_flags_wait_unknown_attribute() {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            r#"provider aws {
    region = "us-east-1"
}
"#,
        ),
        (
            "main.crn",
            r#"let cert = aws.acm.Certificate {
    domain_name       = "registry.example.com"
    validation_method = "DNS"
}
"#,
        ),
        (
            "wait.crn",
            r#"let waited = wait cert {
    until = cert.statu == "ISSUED"
}
"#,
        ),
    ]);

    let diags = cli_validate(&fixture);
    assert!(
        diags
            .iter()
            .any(|d| d.contains("statu") && d.contains("unknown attribute")),
        "validate must surface unknown wait attribute; got: {:?}",
        diags
    );
}

#[test]
fn validate_rejects_non_eq_predicate_operator() {
    // Parser-level rejection — never reaches the differ.
    let fixture = write_fixture(&[
        (
            "providers.crn",
            r#"provider aws {
    region = "us-east-1"
}
"#,
        ),
        (
            "main.crn",
            r#"let cert = aws.acm.Certificate {
    domain_name       = "registry.example.com"
    validation_method = "DNS"
}
"#,
        ),
        (
            "wait.crn",
            r#"let waited = wait cert {
    until = cert.status != "FAILED"
}
"#,
        ),
    ]);

    let diags = cli_validate(&fixture);
    assert!(
        diags.iter().any(|d| d.contains("only `==` is supported")),
        "validate must surface non-`==` operator rejection; got: {:?}",
        diags
    );
}
