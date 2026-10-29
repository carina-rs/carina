//! End-to-end validation regressions for carina#3710.
//!
//! Module output (`attributes {}`) references are validated on expanded
//! compositions, with a source-local fallback when expansion itself fails.
//! These fixtures are deliberately directory-scoped and split declarations
//! across sibling `.crn` files so the test exercises the same loader boundary
//! as a real configuration.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::Value;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
use indexmap::IndexMap;
use tempfile::TempDir;

struct AwsccTestFactory;

impl ProviderFactory for AwsccTestFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC (carina#3710 validation stub)"
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
    ) -> BoxFuture<'_, ProviderResult<Box<dyn Provider>>> {
        Box::pin(async { Ok(Box::new(NoopProvider) as Box<dyn Provider>) })
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
        let schema_drafts: Vec<carina_core::schema::ResourceSchema> = vec![target_group_schema()];
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

fn target_group_schema() -> ResourceSchema {
    ResourceSchema::new("elbv2.TargetGroup")
        .attribute(
            AttributeSchema::required("name", AttributeType::string())
                .create_only()
                .build(),
        )
        .attribute(
            AttributeSchema::provider_populated("target_group_arn", AttributeType::string())
                .build(),
        )
        .with_unique_name_attribute("name")
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
        Box::pin(async { Ok(()) })
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

struct Fixture {
    _temp: TempDir,
    caller: PathBuf,
}

impl Fixture {
    fn direct(module_arguments: &str, module_attributes: &str, caller_body: &str) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let module = temp.path().join("module");
        let caller = temp.path().join("caller");
        std::fs::create_dir(&module).expect("module directory");
        std::fs::create_dir(&caller).expect("caller directory");

        std::fs::write(module.join("arguments.crn"), module_arguments).expect("module arguments");
        std::fs::write(module.join("attributes.crn"), module_attributes)
            .expect("module attributes");
        std::fs::write(
            module.join("resources.crn"),
            r#"let target_group = awscc.elbv2.TargetGroup {
  name = "app"
}
"#,
        )
        .expect("module resources");
        write_provider(&caller);
        std::fs::write(caller.join("main.crn"), caller_body).expect("caller main");

        Self {
            _temp: temp,
            caller,
        }
    }

    fn validate(&self) -> Vec<String> {
        carina_cli::commands::validate::validate_with_factories(&self.caller, factories())
    }
}

fn write_provider(dir: &Path) {
    std::fs::write(
        dir.join("providers.crn"),
        r#"provider awscc {
  region = "us-east-1"
}
"#,
    )
    .expect("provider fixture");
}

fn imported_caller() -> &'static str {
    r#"let component = use { source = '../module' }

let instance = component { }
"#
}

fn assert_unknown_arn(diags: &[String]) {
    assert!(
        diags
            .iter()
            .any(|diag| diag.contains("unknown attribute 'arn'")),
        "expected an unknown-attribute diagnostic, got: {diags:#?}",
    );
}

#[test]
fn unannotated_module_attribute_rejects_unknown_resource_attribute() {
    let fixture = Fixture::direct(
        "",
        "attributes {\n  target_group_arn = target_group.arn\n}\n",
        imported_caller(),
    );

    let diags = fixture.validate();

    assert_unknown_arn(&diags);
    assert!(
        diags.iter().any(|diag| {
            diag.contains("reference target_group.arn")
                && !diag.contains("instance.target_group.arn")
        }),
        "diagnostic should preserve the authored module resource path: {diags:#?}",
    );
}

#[test]
fn primitive_annotated_module_attribute_rejects_unknown_resource_attribute() {
    let fixture = Fixture::direct(
        "",
        "attributes {\n  target_group_arn: String = target_group.arn\n}\n",
        imported_caller(),
    );

    assert_unknown_arn(&fixture.validate());
}

#[test]
fn unannotated_nested_module_attribute_value_rejects_unknown_resource_attribute() {
    let fixture = Fixture::direct(
        "",
        "attributes {\n  target_group_arns = [target_group.arn]\n}\n",
        imported_caller(),
    );

    assert_unknown_arn(&fixture.validate());
}

#[test]
fn unannotated_module_attribute_accepts_existing_resource_attribute() {
    let fixture = Fixture::direct(
        "",
        "attributes {\n  target_group_arn = target_group.target_group_arn\n}\n",
        imported_caller(),
    );

    let diags = fixture.validate();

    assert!(diags.is_empty(), "valid reference should pass: {diags:#?}");
}

#[test]
fn module_attribute_reference_to_module_argument_is_not_a_false_positive() {
    let fixture = Fixture::direct(
        "arguments {\n  supplied: awscc.elbv2.TargetGroup\n}\n",
        "attributes {\n  target_group_arn = supplied.target_group_arn\n}\n",
        r#"let component = use { source = '../module' }

let caller_target_group = awscc.elbv2.TargetGroup {
  name = "caller"
}

let instance = component {
  supplied = caller_target_group
}
"#,
    );

    let diags = fixture.validate();

    assert!(
        diags.is_empty(),
        "module argument references have no local schema binding and must be skipped: {diags:#?}",
    );
}

#[test]
fn nested_import_module_attribute_rejects_unknown_resource_attribute() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inner = temp.path().join("inner");
    let outer = temp.path().join("outer");
    let caller = temp.path().join("caller");
    std::fs::create_dir(&inner).expect("inner directory");
    std::fs::create_dir(&outer).expect("outer directory");
    std::fs::create_dir(&caller).expect("caller directory");

    std::fs::write(
        inner.join("attributes.crn"),
        "attributes {\n  target_group_arn = target_group.arn\n}\n",
    )
    .expect("inner attributes");
    std::fs::write(
        inner.join("resources.crn"),
        r#"let target_group = awscc.elbv2.TargetGroup {
  name = "inner"
}
"#,
    )
    .expect("inner resources");
    std::fs::write(
        outer.join("attributes.crn"),
        "attributes {\n  marker = \"outer\"\n}\n",
    )
    .expect("outer attributes");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner_module = use { source = '../inner' }

let inner_instance = inner_module { }
"#,
    )
    .expect("outer main");
    write_provider(&caller);
    std::fs::write(
        caller.join("main.crn"),
        r#"let outer_module = use { source = '../outer' }

let outer_instance = outer_module { }
"#,
    )
    .expect("caller main");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert_unknown_arn(&diags);
    assert!(
        diags.iter().any(|diag| {
            diag.contains("reference target_group.arn")
                && !diag.contains("outer_instance.inner_instance.target_group.arn")
        }),
        "nested diagnostic should preserve the authored resource path: {diags:#?}",
    );
}

#[test]
fn cyclic_import_attribute_validation_reports_bad_ref_once() {
    let temp = tempfile::tempdir().expect("tempdir");
    let module_a = temp.path().join("a");
    let module_b = temp.path().join("b");
    let caller = temp.path().join("caller");
    std::fs::create_dir(&module_a).expect("module a directory");
    std::fs::create_dir(&module_b).expect("module b directory");
    std::fs::create_dir(&caller).expect("caller directory");

    std::fs::write(
        module_a.join("attributes.crn"),
        "attributes {\n  target_group_arn = target_group.arn\n}\n",
    )
    .expect("module a attributes");
    std::fs::write(
        module_a.join("resources.crn"),
        r#"let target_group = awscc.elbv2.TargetGroup {
  name = "a"
}
"#,
    )
    .expect("module a resources");
    std::fs::write(
        module_a.join("main.crn"),
        r#"let b_module = use { source = '../b' }

let b_instance = b_module { }
"#,
    )
    .expect("module a main");

    std::fs::write(
        module_b.join("attributes.crn"),
        "attributes {\n  marker = \"b\"\n}\n",
    )
    .expect("module b attributes");
    std::fs::write(
        module_b.join("main.crn"),
        r#"let a_module = use { source = '../a' }

let a_instance = a_module { }
"#,
    )
    .expect("module b main");

    write_provider(&caller);
    std::fs::write(
        caller.join("main.crn"),
        r#"let a_module = use { source = '../a' }

let a_instance = a_module { }
"#,
    )
    .expect("caller main");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());
    let bad_ref_count = diags
        .iter()
        .filter(|diag| diag.contains("unknown attribute 'arn'"))
        .count();

    assert_eq!(
        bad_ref_count, 1,
        "cycle guard should report the bad reference exactly once: {diags:#?}",
    );
}
