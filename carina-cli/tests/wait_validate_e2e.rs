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
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
};
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
            Vec::new(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<carina_core::schema::ValidatedResourceSchema> {
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = {
            vec![
                cert_schema(),
                cert_data_source_schema(),
                nested_holder_schema(),
            ]
        };
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
    let status_enum = AttributeType::enum_(
        TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Status"),
        Some(vec!["ISSUED".to_string()]),
        vec![("ISSUED".to_string(), "issued".to_string())],
        None,
        None,
    );
    ResourceSchema::new("acm.Certificate")
        .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("validation_method", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("status", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("enum_status", status_enum).build())
        .attribute(AttributeSchema::optional("certificate_arn", AttributeType::string()).build())
}

fn cert_data_source_schema() -> ResourceSchema {
    ResourceSchema::new("acm.Certificate")
        .as_data_source()
        .attribute(AttributeSchema::optional("domain_name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("status", AttributeType::string()).build())
}

fn nested_holder_schema() -> ResourceSchema {
    ResourceSchema::new("test.NestedHolder").attribute(
        AttributeSchema::optional(
            "inner",
            AttributeType::struct_(
                "CertificateRef".to_string(),
                vec![StructField::optional("certificate_arn", AttributeType::string()).build()],
            ),
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
        Box::pin(async move {
            Ok(carina_core::resource::State::<
                carina_core::resource::NoPlaceholder,
            >::existing(id, HashMap::new()))
        })
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
            Ok(carina_core::provider::UpdateOutcome::success(
                carina_core::provider::AppliedState::new(id, identifier, HashMap::new())
                    .expect("provider input identifier is non-empty"),
            ))
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
        Box::new(WaitTestFactory) as Box<dyn ProviderFactory>
    ])
}

fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, content) in files {
        std::fs::write(dir.path().join(name), content).expect("write fixture");
    }
    dir
}

fn cli_validate(fixture: &TempDir) -> Vec<String> {
    carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        factories(),
    )
}

#[test]
fn migrated_diagnostics_render_parser_owned_sibling_locations() {
    for (source, message, token, last) in [
        (
            "aws.acm.Certificate { domain_name = 'consumer'\n  directives { depends_on = [missing] }\n}\n",
            "binding 'missing' is not declared",
            "missing",
            false,
        ),
        (
            "let ready = wait absent {\n  until = absent.status == 'ready'\n}\n",
            "not a known resource",
            "absent",
            false,
        ),
        (
            "let ready = wait cert {\n  until = cert.statu == 'ready'\n}\n",
            "unknown attribute",
            "cert.statu",
            false,
        ),
        (
            "let ready = wait cert {\n  until = cert.status == ready\n}\n",
            "cannot resolve `ready`",
            "ready",
            true,
        ),
        (
            "aws.acm.Certificate { domain_name = 'consumer'\n  directives { provider = typo }\n}\n",
            "`typo` is not a known provider instance",
            "typo",
            false,
        ),
    ] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider aws {}\n"),
            (
                "resources.crn",
                "let cert = aws.acm.Certificate { domain_name = 'example.com' }\n",
            ),
            ("sibling.crn", source),
        ]);
        let errors = cli_validate(&fixture);
        let matching: Vec<_> = errors
            .iter()
            .filter(|error| error.contains(message))
            .collect();
        assert_eq!(matching.len(), 1, "{message}: {errors:#?}");
        let start = if last {
            source.rfind(token)
        } else {
            source.find(token)
        }
        .unwrap();
        let prefix = &source[..start];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() + 1;
        let column = prefix.rsplit('\n').next().unwrap().chars().count() + 1;
        assert!(
            matching[0].contains(&format!("sibling.crn:{line}:{column}:")),
            "{message}: {errors:#?}"
        );
    }
}

#[test]
fn module_wait_diagnostic_renders_the_authored_file_after_expansion() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider aws {}\n"),
        (
            "main.crn",
            "let module = use { source = './child' }\nlet instance = module {}\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(
        fixture.path().join("child/main.crn"),
        "arguments { label: String = 'module' }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("child/resources.crn"),
        "let cert = aws.acm.Certificate { domain_name = 'module.example.com' }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("child/wait.crn"),
        "let ready = wait cert {\n  until = cert.statu == 'ready'\n}\n",
    )
    .unwrap();
    let errors = cli_validate(&fixture);
    assert!(
        errors
            .iter()
            .any(|error| error.contains("child/wait.crn:2:11: wait")
                && error.contains("unknown attribute `instance.cert.statu`")),
        "{errors:#?}"
    );
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

fn wait_depends_on_fixture(dependency: &str, sibling: &str) -> TempDir {
    write_fixture(&[
        ("providers.crn", "provider aws {}\n"),
        (
            "resources.crn",
            &format!(
                "let cert = aws.acm.Certificate {{ domain_name = \"example.com\" }}\n{sibling}\n"
            ),
        ),
        (
            "wait.crn",
            &format!(
                "let ready = wait cert {{\n    until = cert.status == \"ISSUED\"\n    depends_on = [{dependency}]\n}}\n"
            ),
        ),
    ])
}

#[test]
fn validate_rejects_wait_unknown_depends_on_in_multi_file_directory() {
    let fixture = wait_depends_on_fixture("nope", "");
    let diagnostics = cli_validate(&fixture);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("binding 'nope' is not declared in this scope")),
        "validate must reject the unknown wait dependency: {diagnostics:?}"
    );
}

fn assert_wait_depends_on_rejections(cases: &[(&str, &str, &str)]) {
    let mut failures = Vec::new();
    for &(dependency, declaration, expected_message) in cases {
        let fixture = wait_depends_on_fixture(dependency, declaration);
        let diagnostics = cli_validate(&fixture);
        if !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains(expected_message))
        {
            failures.push((dependency, expected_message, diagnostics));
        }
    }
    assert!(
        failures.is_empty(),
        "validate must reject each wait dependency with the expected diagnostic: {failures:#?}"
    );
}

#[test]
fn validate_rejects_wait_non_resource_depends_on_in_multi_file_directory() {
    assert_wait_depends_on_rejections(&[
        (
            "helper",
            "fn helper() { 1 }",
            "wait.depends_on on 'ready': binding 'helper' is not declared in this scope",
        ),
        (
            "rec",
            "arguments { rec: String }",
            "wait.depends_on: expected a managed resource handle; got String",
        ),
    ]);
}

#[test]
fn validate_keeps_parser_rejections_for_wait_depends_on_in_multi_file_directory() {
    assert_wait_depends_on_rejections(&[
        (
            "upstream",
            "let upstream = upstream_state { source = './upstream' }",
            "wait.depends_on: expected a managed resource handle; got an upstream state",
        ),
        (
            "secondary",
            "let secondary = provider aws {}",
            "wait.depends_on: expected a managed resource handle; got a provider instance",
        ),
        (
            "imported",
            "let imported = use { source = './module' }",
            "wait.depends_on: expected a managed resource handle; got a module import",
        ),
        (
            "other",
            "let other = wait cert { until = cert.status == 'ISSUED' }",
            "wait.depends_on: expected a managed resource handle; got a wait binding",
        ),
    ]);
}

#[test]
fn validate_accepts_wait_depends_on_resource_and_data_source_in_sibling_file() {
    for (dependency, sibling) in [
        (
            "dependency",
            r#"let dependency = aws.acm.Certificate { domain_name = "dependency.example.com" }"#,
        ),
        (
            "lookup",
            r#"let lookup = read aws.acm.Certificate { domain_name = "lookup.example.com" }"#,
        ),
    ] {
        let fixture = wait_depends_on_fixture(dependency, sibling);
        let diagnostics = cli_validate(&fixture);
        assert!(diagnostics.is_empty(), "{dependency}: {diagnostics:?}");
    }
}

#[test]
fn validate_rejects_wait_unknown_depends_on_in_standalone_module_directory() {
    let fixture = write_fixture(&[
        ("arguments.crn", "arguments { domain: String }\n"),
        (
            "resources.crn",
            "let cert = aws.acm.Certificate { domain_name = domain }\n",
        ),
        (
            "wait.crn",
            r#"let ready = wait cert {
    until = cert.status == "ISSUED"
    depends_on = [nope]
}
"#,
        ),
    ]);
    let diagnostics = cli_validate(&fixture);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains("binding 'nope' is not declared in this scope")),
        "standalone module validation must reject the unknown wait dependency: {diagnostics:?}"
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
fn validate_rejects_data_source_wait_target_from_sibling_file() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider aws {}\n"),
        (
            "data.crn",
            r#"let lookup = read aws.acm.Certificate {
    domain_name = "example.com"
}
"#,
        ),
        (
            "main.crn",
            r#"let found = wait lookup {
    until = lookup.status == "ISSUED"
}
"#,
        ),
    ]);

    let diagnostics = cli_validate(&fixture);
    let expected = format!(
        "{}:1:18: wait `found`: target binding `lookup` is a data source; wait targets must be managed resources",
        fixture.path().join("main.crn").display(),
    );
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic == &expected),
        "validate must reject a sibling data source as a wait target; got: {diagnostics:?}"
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
fn validate_requires_quotes_for_unresolved_wait_enum_identifiers() {
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
            r#"let state_ready = wait cert {
    until = cert.status == ready
}

let enum_ready = wait cert {
    until = cert.enum_status == aws.acm.Certificate.Status.bogus
}
"#,
        ),
    ]);

    let diagnostics = cli_validate(&fixture);
    assert!(diagnostics.iter().any(|diagnostic| diagnostic.contains(
        "wait `state_ready`: cannot resolve `ready` as an enum value of `cert.status`; quote it to compare as a string"
    )), "validate must reject an unquoted identifier on a string attribute; got: {diagnostics:?}");
    assert!(diagnostics.iter().any(|diagnostic| diagnostic.contains(
        "wait `enum_ready`: cannot resolve `aws.acm.Certificate.Status.bogus` as an enum value of `cert.enum_status`; quote it to compare as a string"
    )), "validate must reject an unknown enum member; got: {diagnostics:?}");
}

#[test]
fn validate_flags_wait_binding_bad_attr_inside_nested_struct() {
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
    domain_name       = "publish.example.com"
    validation_method = "DNS"
}

let holder = aws.test.NestedHolder {
    inner = { certificate_arn = cert_issued.arn }
}
"#,
        ),
        (
            "wait.crn",
            r#"let cert_issued = wait cert {
    until = cert.status == "ISSUED"
}
"#,
        ),
    ]);

    let diags = cli_validate(&fixture);
    assert!(
        diags
            .iter()
            .any(|d| { d.contains("unknown attribute 'arn'") && d.contains("cert_issued") }),
        "validate must surface wait binding bad attr inside nested struct; got: {:?}",
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
