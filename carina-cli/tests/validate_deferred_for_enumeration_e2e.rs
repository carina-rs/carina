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
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
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
            vec![cert_schema(), record_set_schema()];
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

fn cert_schema() -> ResourceSchema {
    let resource_record = AttributeType::struct_(
        "ResourceRecord",
        vec![
            StructField::optional("name", AttributeType::string()).build(),
            StructField::optional("value", AttributeType::string()).build(),
        ],
    );
    let validation_option = AttributeType::struct_(
        "DomainValidationOption",
        vec![StructField::optional("resource_record", resource_record).build()],
    );
    ResourceSchema::new("acm.Certificate")
        .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("validation_method", AttributeType::string()).build())
        // Provider-read/computed collection that the `for` iterates.
        .attribute(
            AttributeSchema::optional(
                "domain_validation_options",
                AttributeType::list(validation_option),
            )
            .build(),
        )
}

fn record_set_schema() -> ResourceSchema {
    ResourceSchema::new("route53.RecordSet")
        .attribute(AttributeSchema::optional("hosted_zone_id", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("type", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("ttl", AttributeType::int()).build())
        .attribute(
            AttributeSchema::optional(
                "resource_records",
                AttributeType::list(AttributeType::string()),
            )
            .build(),
        )
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "aws"
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
        Box::new(ForTestFactory) as Box<dyn ProviderFactory>
    ])
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
            r#"for (_, opt) in cert.domain_validation_options {
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
    // carina#4109: an unbound loop has no synthetic live binding. Pin the
    // pending placeholder plus its authored source location instead.
    assert!(
        ids.iter().any(|id| id.contains("route53.RecordSet")
            && id.contains(".<pending>")
            && id.contains("(deferred for at records.crn:1)")),
        "the for-loop body (aws.route53.RecordSet) over a same-config \
         read attribute must be enumerated as a deferred placeholder \
         entry (`route53.RecordSet.<pending> (deferred for at <file>:<line>)`); \
         got: {:?}",
        ids
    );
}

#[test]
fn validate_lists_authored_module_if_iterable_expression() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"arguments {
  flag: Bool
  enabled: list(String)
  disabled: list(String)
}
let names = if flag { enabled } else { disabled }
let resources = for name in names {
  aws.route53.RecordSet { name = name }
}
"#,
    )]);

    let ids = carina_cli::commands::validate::validated_resource_ids_with_factories(
        fixture.path(),
        factories(),
    );
    let listing = ids.join("\n");
    assert!(
        listing.contains("deferred: for name in names"),
        "validate did not preserve the authored iterable expression: {listing}",
    );
    assert!(!listing.contains("unresolved module argument"), "{listing}");
}

#[test]
fn validate_reports_runtime_deferred_module_call_body_at_resolution_seam() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider aws { region = 'us-east-1' }\n"),
        (
            "main.crn",
            "let child = use { source = './child' }\n\
             let cert = aws.acm.Certificate { domain_name = 'example.com' }\n\
             let items = for item in cert.domain_validation_options { child { item = item } }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).expect("child directory");
    std::fs::write(
        fixture.path().join("child/main.crn"),
        "arguments { item: String }\naws.route53.RecordSet { name = item }\n",
    )
    .expect("child module");

    let errors =
        carina_cli::commands::validate::validate_with_factories(fixture.path(), factories());
    assert!(
        errors.iter().any(|error| {
            error.contains("for item in cert.domain_validation_options")
                && error.contains("module call `child`")
                && error.contains("line 3")
                && error.contains("still unresolved after module expansion")
        }),
        "carina validate must report the typed deferred-body error: {errors:#?}"
    );
}

#[test]
fn validate_reports_runtime_deferred_read_body_at_resolution_seam() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider aws { region = 'us-east-1' }\n"),
        (
            "main.crn",
            "let cert = aws.acm.Certificate { domain_name = 'example.com' }\n\
             let items = for item in cert.domain_validation_options {\n\
               read aws.route53.RecordSet { name = item }\n\
             }\n",
        ),
    ]);

    let errors =
        carina_cli::commands::validate::validate_with_factories(fixture.path(), factories());
    assert!(
        errors.iter().any(|error| {
            error.contains("for item in cert.domain_validation_options")
                && error.contains("read `aws.route53.RecordSet`")
                && error.contains("line 2")
                && error.contains("still unresolved after module expansion")
        }),
        "carina validate must report the typed deferred-body error: {errors:#?}"
    );
}

#[test]
fn validate_reports_imported_deferred_body_at_owning_call() {
    let fixture = write_fixture(&[(
        "caller.crn",
        "let parent = use { source = './parent' }\n\
         let instance = parent { names = ['one'] }\n",
    )]);
    std::fs::create_dir_all(fixture.path().join("parent/grandchild"))
        .expect("nested module directories");
    std::fs::write(
        fixture.path().join("parent/main.crn"),
        "arguments { names: list(String) }\n\
         let grandchild = use { source = './grandchild' }\n\
         let children = for name in names { grandchild { name = name } }\n",
    )
    .expect("parent module");
    std::fs::write(
        fixture.path().join("parent/grandchild/main.crn"),
        "arguments { name: String }\n",
    )
    .expect("grandchild module");

    let errors =
        carina_cli::commands::validate::validate_with_factories(fixture.path(), factories());
    let error = errors
        .iter()
        .find(|error| error.contains("cannot use module call `grandchild`"))
        .unwrap_or_else(|| panic!("missing deferred-body error: {errors:#?}"));
    assert!(
        error.contains("caller.crn") && error.contains("line 2"),
        "the error must identify the root module call that owns the imported rejection: {error}"
    );
}

#[test]
fn validate_reports_every_rejected_loop_across_files() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider aws { region = 'us-east-1' }\n"),
        (
            "main.crn",
            "let child = use { source = './child' }\n\
             let cert = aws.acm.Certificate { domain_name = 'example.com' }\n",
        ),
        (
            "first.crn",
            "let first = for item in cert.domain_validation_options { child { item = item } }\n",
        ),
        (
            "second.crn",
            "let second = for item in cert.domain_validation_options { child { item = item } }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).expect("child directory");
    std::fs::write(
        fixture.path().join("child/main.crn"),
        "arguments { item: String }\n",
    )
    .expect("child module");

    let errors =
        carina_cli::commands::validate::validate_with_factories(fixture.path(), factories());
    let rejected = errors
        .iter()
        .filter(|error| error.contains("only a managed resource body can be deferred"))
        .collect::<Vec<_>>();
    assert_eq!(rejected.len(), 2, "errors: {errors:#?}");
    assert!(rejected.iter().any(|error| error.contains("first.crn")));
    assert!(rejected.iter().any(|error| error.contains("second.crn")));
}
