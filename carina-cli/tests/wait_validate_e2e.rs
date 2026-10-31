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
use carina_core::resource::Value;
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
};
use indexmap::IndexMap;
use std::collections::HashMap;
use std::process::{Command, Output};
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
        Box::new(WaitTestFactory) as Box<dyn ProviderFactory>
    ])
}

fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create fixture directory");
        }
        std::fs::write(path, content).expect("write fixture");
    }
    dir
}

fn cli_validate(fixture: &TempDir) -> Vec<String> {
    carina_cli::commands::validate::validate_with_factories(fixture.path(), factories())
}

fn carina_validate(fixture: &TempDir) -> Output {
    let home = tempfile::tempdir().expect("isolated carina home");
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", "."])
        .current_dir(fixture.path())
        .env("NO_COLOR", "1")
        .env("HOME", home.path())
        .env("CARINA_TEST_HOME", home.path())
        .env("CARINA_TRUST_RECORD_DIR", home.path().join("trust"))
        .env("CARINA_PLUGIN_CACHE_DIR", home.path().join("plugins"))
        .output()
        .expect("run carina validate")
}

fn rendered_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn validate_rejects_unknown_wait_dependency_across_sibling_files() {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
        ),
        (
            "resource.crn",
            r#"let d = mock.test.resource {
    name = "d"
}
"#,
        ),
        (
            "wait.crn",
            r#"let w = wait d {
    until = d.comment == "x"
    depends_on = [nope]
}
"#,
        ),
    ]);

    let output = carina_validate(&fixture);
    let rendered = rendered_output(&output);
    assert!(
        !output.status.success(),
        "unknown wait dependency must fail carina validate: {rendered}"
    );
    assert!(
        rendered.contains("wait 'w' depends_on: binding 'nope' is not declared in this scope"),
        "carina validate must name the unknown wait dependency: {rendered}"
    );
}

#[test]
fn validate_module_wait_dependency_keeps_the_authored_unknown_name() {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
        ),
        (
            "main.crn",
            r#"let reusable = use { source = "./module" }
let instance = reusable { label = "caller" }
"#,
        ),
        ("module/arguments.crn", "arguments { label: String }\n"),
        (
            "module/resource.crn",
            r#"let d = mock.test.resource {
    name = "d"
}
"#,
        ),
        (
            "module/wait.crn",
            r#"let w = wait d {
    until = d.comment == "x"
    depends_on = [nope]
}
"#,
        ),
    ]);

    let output = carina_validate(&fixture);
    let rendered = rendered_output(&output);
    assert!(
        !output.status.success(),
        "unknown module wait dependency must fail carina validate: {rendered}"
    );
    assert!(
        rendered.contains("wait 'w' depends_on: binding 'nope' is not declared in this scope"),
        "module diagnostic must preserve the authored name: {rendered}"
    );
    assert!(
        !rendered.contains("instance.nope"),
        "module expansion must not invent a prefixed unknown binding: {rendered}"
    );
}

#[test]
fn validate_accepts_resource_and_wait_dependencies_on_module_call_binding() {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            r#"backend local { path = "carina.state.json" }
provider mock {}
"#,
        ),
        (
            "main.crn",
            r#"let reusable = use { source = "./module" }
let instance = reusable { label = "caller" }
"#,
        ),
        (
            "consumer.crn",
            r#"let local = mock.test.resource {
    name = "local"
    directives { depends_on = [instance] }
}
let w = wait local {
    until = local.comment == "ready"
    depends_on = [instance]
}
"#,
        ),
        ("module/arguments.crn", "arguments { label: String }\n"),
        (
            "module/resource.crn",
            r#"let inner = mock.test.resource {
    name = label
}
attributes {
    name = inner.name
}
"#,
        ),
    ]);

    let diagnostics = cli_validate(&fixture);
    assert!(
        diagnostics
            .iter()
            .all(|message| !message.contains("binding 'instance' is not declared")),
        "declared module-call/composition bindings remain valid dependency targets: {diagnostics:?}"
    );
}

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
    assert!(
        diagnostics.iter().any(|diagnostic| diagnostic
            == "wait `found`: target binding `lookup` is a data source; wait targets must be managed resources"),
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
