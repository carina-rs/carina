//! End-to-end: `carina validate` must enumerate the body of a `for`
//! loop whose iterable is a same-config provider-read attribute
//! (carina#3121, design PR-1 / fix A).
//!
//! Before this fix, `validate` reported resources off `parsed.resources`
//! directly. A `for opt in cert.domain_validation_options { ... }` loop
//! over a same-config `let cert` is parsed into a `DeferredForExpression`
//! and contributes *zero* entries to `parsed.resources`, so the loop
//! body silently vanished from the `✓ N resources validated` count and
//! list. The fix routes the display through
//! `ParsedFile::iter_all_resources()` (the unified-walk invariant from
//! `notes/specs/2026-04-19-unify-resource-walk-design.md`), which
//! already yields every `DeferredForExpression.template_resource`.
//!
//! Per CLAUDE.md "directory-scoped, never single-file": the fixture is
//! a multi-file `tempfile::tempdir()` layout mirroring the real
//! `infra/.../registry` shape — `cert` is declared in `main.crn`, the
//! loop body in `records.crn`, the provider in `providers.crn` — so we
//! exercise the merged-parse path the CLI and LSP share, not a single
//! buffer.

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{DataSource, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;
use std::collections::HashMap;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Minimal aws provider factory exposing the two resource types the
// fixture references so the validate pipeline sees real types.
// ---------------------------------------------------------------------------

struct ForTestFactory;

impl ProviderFactory for ForTestFactory {
    fn name(&self) -> &str {
        "aws"
    }
    fn display_name(&self) -> &str {
        "AWS (for-loop test stub)"
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
        vec![cert_schema(), record_set_schema()]
    }
}

fn cert_schema() -> ResourceSchema {
    ResourceSchema::new("acm.Certificate")
        .attribute(AttributeSchema::new("domain_name", AttributeType::String))
        .attribute(AttributeSchema::new(
            "validation_method",
            AttributeType::String,
        ))
        // Provider-read/computed collection that the `for` iterates.
        // Element type is immaterial here — the loop is deferred at
        // parse time because `cert.domain_validation_options` is an
        // unresolved provider-computed reference, not because of the
        // element schema.
        .attribute(AttributeSchema::new(
            "domain_validation_options",
            AttributeType::List {
                inner: Box::new(AttributeType::String),
                ordered: true,
            },
        ))
}

fn record_set_schema() -> ResourceSchema {
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
            AttributeType::List {
                inner: Box::new(AttributeType::String),
                ordered: true,
            },
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
    vec![Box::new(ForTestFactory) as Box<dyn ProviderFactory>]
}

fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).expect("write fixture");
    }
    dir
}

/// The real registry shape: `cert` (a `let`-bound same-config resource)
/// in one file, a `for` over its provider-read `domain_validation_options`
/// in a sibling file.
fn registry_like_fixture() -> TempDir {
    write_fixture(&[
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
            "records.crn",
            r#"for _, opt in cert.domain_validation_options {
    aws.route53.RecordSet {
        hosted_zone_id   = "Z123"
        name             = opt.resource_record.name
        type             = "CNAME"
        ttl              = 300
        resource_records = [opt.resource_record.value]
    }
}
"#,
        ),
    ])
}

#[test]
fn validate_enumerates_deferred_for_body_over_same_config_read_attr() {
    let fixture = registry_like_fixture();
    let ids = carina_cli::commands::validate::validated_resource_ids_with_factories(
        fixture.path(),
        factories(),
    );

    // The same-config `let cert` resolves at parse time and is a direct
    // resource.
    assert!(
        ids.iter()
            .any(|id| id.contains("acm") && id.contains("cert")),
        "the let-bound certificate must be enumerated; got: {:?}",
        ids
    );

    // The loop body is the regression target: before fix A it was
    // entirely absent from the list. It must now appear, tagged as
    // deferred, so the user can see the resource the planner intends
    // to manage instead of silent data loss.
    // Pin the actual feature shape, not just loose substrings: the
    // placeholder address form `{type}.{binding}[?]`, the
    // `(deferred: <header>)` suffix, AND the `@ <file>:<line>`
    // location (which keeps two distinct loops over the same iterable
    // distinguishable) are the whole point of fix A — a regression
    // that dropped any of them must fail this test.
    assert!(
        ids.iter().any(|id| id.contains("route53.RecordSet")
            && id.contains("[?]")
            && id.contains("(deferred:")
            && id.contains(" @ ")),
        "the for-loop body (aws.route53.RecordSet) over a same-config \
         read attribute must be enumerated as a deferred placeholder \
         entry (`route53.RecordSet…[?] (deferred: … @ <file>:<line>)`); \
         got: {:?}",
        ids
    );
}
