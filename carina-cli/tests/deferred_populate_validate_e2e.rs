//! End-to-end validation tests for the `deferred_populate` annotation
//! (carina#3034). Mirrors the directory-scoped pattern of
//! `wait_validate_e2e.rs`.
//!
//! Per CLAUDE.md "directory-scoped, never single-file": every fixture
//! is a multi-file `tempfile::tempdir()` layout — the cert lives in
//! `acm.crn`, the dependent route53 RecordSet in `route53.crn`, and
//! the optional `wait` block in `wait.crn`, so the merged-parse path
//! that LSP and CLI share is exercised.

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{Resource, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
use indexmap::IndexMap;
use std::collections::HashMap;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Test factory exposing aws.acm.Certificate (with a deferred-populate
// inner field on `domain_validation_options`) and aws.route53.RecordSet.
// ---------------------------------------------------------------------------

struct DeferredPopulateTestFactory;

impl ProviderFactory for DeferredPopulateTestFactory {
    fn name(&self) -> &str {
        "aws"
    }
    fn display_name(&self) -> &str {
        "AWS (deferred-populate test stub)"
    }
    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }
    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }
    fn validate_custom_type(&self, _type_name: &str, _value: &str) -> Result<(), String> {
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
        vec![cert_schema(), rrset_schema()]
    }
}

/// Mirrors the post-aws#296 `acm.Certificate` schema shape:
/// `domain_validation_options` uses the *read-side* struct
/// (`DomainValidation` from `CertificateDetail`), with a nested
/// `resource_record: { name, type, value }` substruct that is
/// marked `.deferred_populate()` because ACM populates it
/// asynchronously after `RequestCertificate` returns.
///
/// This is the shape that lets carina#3032's failing chained
/// access (`cert.domain_validation_options[0].resource_record.value`)
/// be flagged at validate time by carina#3036's deferred-populate
/// rule.
fn cert_schema() -> ResourceSchema {
    ResourceSchema::new("acm.Certificate")
        .attribute(AttributeSchema::new("domain_name", AttributeType::String))
        .attribute(AttributeSchema::new(
            "validation_method",
            AttributeType::String,
        ))
        .attribute(AttributeSchema::new("status", AttributeType::String).deferred_populate())
        .attribute(AttributeSchema::new(
            "domain_validation_options",
            AttributeType::list(AttributeType::Struct {
                name: "DomainValidation".to_string(),
                fields: vec![
                    StructField::new("domain_name", AttributeType::String),
                    StructField::new(
                        "resource_record",
                        AttributeType::Struct {
                            name: "ResourceRecord".to_string(),
                            fields: vec![
                                StructField::new("name", AttributeType::String),
                                StructField::new("type", AttributeType::String),
                                StructField::new("value", AttributeType::String),
                            ],
                        },
                    )
                    .deferred_populate(),
                    StructField::new("validation_status", AttributeType::String),
                ],
            }),
        ))
}

fn rrset_schema() -> ResourceSchema {
    ResourceSchema::new("route53.RecordSet")
        .attribute(AttributeSchema::new(
            "hosted_zone_id",
            AttributeType::String,
        ))
        .attribute(AttributeSchema::new("name", AttributeType::String))
        .attribute(AttributeSchema::new("type", AttributeType::String))
        .attribute(AttributeSchema::new("ttl", AttributeType::Int))
        .attribute(AttributeSchema::new(
            "resource_records",
            AttributeType::list(AttributeType::String),
        ))
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
        resource: &Resource,
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
    vec![Box::new(DeferredPopulateTestFactory) as Box<dyn ProviderFactory>]
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

const PROVIDERS_CRN: &str = r#"provider aws {
    region = "us-east-1"
}
"#;

const ACM_CRN: &str = r#"let cert = aws.acm.Certificate {
    domain_name       = "registry.example.com"
    validation_method = "DNS"
}
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The carina#3032 reproduction shape, decomposed into a multi-file
/// fixture matching the post-aws#297 schema. The cert is in
/// `acm.crn`, the route53 RecordSet that reads the cert's
/// deferred-populate inner struct is in `route53.crn`, and there is
/// *no* `wait` block — validate must flag the chained reference
/// and name `cert` as the wait target.
#[test]
fn validate_flags_chained_ref_to_deferred_inner_field_without_wait() {
    let fixture = write_fixture(&[
        ("providers.crn", PROVIDERS_CRN),
        ("acm.crn", ACM_CRN),
        (
            "route53.crn",
            r#"aws.route53.RecordSet {
    hosted_zone_id   = "Z1"
    name             = cert.domain_validation_options[0].resource_record.name
    type             = "CNAME"
    ttl              = 300
    resource_records = [cert.domain_validation_options[0].resource_record.value]
}
"#,
        ),
    ]);

    let diags = cli_validate(&fixture);
    assert!(
        diags.iter().any(
            |d| d.contains("cert.domain_validation_options[0].resource_record.value")
                && d.contains("wait cert")
        ),
        "expected a deferred-populate diagnostic naming the unresolved \
         path and `wait cert` as the workaround; got: {diags:?}",
    );
}

/// Adding `wait cert { ... }` in a sibling `.crn` file satisfies the
/// rule for *any* chained access on `cert` — the LSP and CLI both
/// merge the directory before validating, so a wait declared in
/// `wait.crn` is visible to references in `route53.crn`. This is
/// the load-bearing directory-scoped assertion for the feature, and
/// the actual fix for carina#3032's failing real-infra shape.
#[test]
fn validate_accepts_chained_ref_when_wait_is_declared_in_sibling_file() {
    let fixture = write_fixture(&[
        ("providers.crn", PROVIDERS_CRN),
        ("acm.crn", ACM_CRN),
        (
            "route53.crn",
            r#"aws.route53.RecordSet {
    hosted_zone_id   = "Z1"
    name             = cert.domain_validation_options[0].resource_record.name
    type             = "CNAME"
    ttl              = 300
    resource_records = [cert.domain_validation_options[0].resource_record.value]
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
        !diags
            .iter()
            .any(|d| d.contains("domain_validation_options")),
        "wait `cert` declared in a sibling file must satisfy the \
         deferred-populate rule for chained refs on the same binding; \
         got: {diags:?}",
    );
}

/// Reference to a non-deferred attribute (`cert.domain_name`) must
/// not trip the diagnostic, even without a `wait`. Sanity check that
/// the rule only fires for schema-flagged paths.
#[test]
fn validate_does_not_flag_non_deferred_attribute_access() {
    let fixture = write_fixture(&[
        ("providers.crn", PROVIDERS_CRN),
        ("acm.crn", ACM_CRN),
        (
            "consumer.crn",
            r#"aws.route53.RecordSet {
    hosted_zone_id   = "Z1"
    name             = cert.domain_name
    type             = "A"
    ttl              = 60
    resource_records = ["1.2.3.4"]
}
"#,
        ),
    ]);

    let diags = cli_validate(&fixture);
    assert!(
        !diags.iter().any(|d| d.contains("populated asynchronously")),
        "non-deferred attribute access must not trip the rule; got: {diags:?}",
    );
}
