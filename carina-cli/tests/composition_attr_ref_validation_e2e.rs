//! End-to-end validation regressions for carina#3712.
//!
//! A module-call binding expands into a composition whose statically known
//! attribute surface comes from the imported module's `attributes {}` block.
//! These fixtures keep the caller split across sibling `.crn` files so the
//! tests exercise the real directory-scoped load and validation boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{DataSource, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
use indexmap::IndexMap;
use tempfile::TempDir;

struct AwsccTestFactory;

impl ProviderFactory for AwsccTestFactory {
    fn name(&self) -> &str {
        "awscc"
    }

    fn display_name(&self) -> &str {
        "AWSCC (carina#3712 validation stub)"
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
        vec![target_group_schema(), consumer_schema(), policy_schema()]
    }
}

fn target_group_schema() -> ResourceSchema {
    let tags = AttributeType::struct_(
        "Tags",
        vec![StructField::new("environment", AttributeType::string())],
    );
    ResourceSchema::new("elbv2.TargetGroup")
        .attribute(
            AttributeSchema::new("name", AttributeType::string())
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("target_group_arn", AttributeType::string()).read_only())
        .attribute(AttributeSchema::new("tags", tags).read_only())
        .with_unique_name_attribute("name")
}

fn consumer_schema() -> ResourceSchema {
    ResourceSchema::new("test.Consumer")
        .attribute(
            AttributeSchema::new("name", AttributeType::string())
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("target_group_arn", AttributeType::string()).required())
        .with_unique_name_attribute("name")
}

fn policy_schema() -> ResourceSchema {
    let statement = AttributeType::struct_(
        "Statement",
        vec![StructField::new(
            "principal",
            AttributeType::union(vec![AttributeType::string(), AttributeType::int()]),
        )],
    );
    let policy_document = AttributeType::struct_(
        "PolicyDocument",
        vec![StructField::new(
            "statement",
            AttributeType::list(statement),
        )],
    );

    ResourceSchema::new("iam.Policy")
        .attribute(
            AttributeSchema::new("name", AttributeType::string())
                .required()
                .create_only(),
        )
        .attribute(AttributeSchema::new("policy_document", policy_document).read_only())
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
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::CreateOutcome>> {
        let id = id.clone();
        Box::pin(async move {
            Ok(carina_core::provider::CreateOutcome::Success {
                state: carina_core::resource::State::existing(id, HashMap::new()),
            })
        })
    }

    fn update(
        &self,
        id: &carina_core::resource::ResourceId,
        _identifier: &str,
        _request: carina_core::provider::UpdateRequest,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::UpdateOutcome>> {
        let id = id.clone();
        Box::pin(async move {
            Ok(carina_core::provider::UpdateOutcome::Success {
                state: carina_core::resource::State::existing(id, HashMap::new()),
            })
        })
    }

    fn delete(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &str,
        _request: carina_core::provider::DeleteRequest,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn required_permissions(
        &self,
        _id: &carina_core::resource::ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> Vec<String> {
        Vec::new()
    }
}

fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(AwsccTestFactory) as Box<dyn ProviderFactory>]
}

struct Fixture {
    _temp: TempDir,
    caller: PathBuf,
}

impl Fixture {
    fn direct(caller_files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let module = temp.path().join("module");
        let caller = temp.path().join("caller");
        std::fs::create_dir(&module).expect("module directory");
        std::fs::create_dir(&caller).expect("caller directory");

        std::fs::write(
            module.join("attributes.crn"),
            "attributes {\n  target_group_arn = target_group.target_group_arn\n}\n",
        )
        .expect("module attributes");
        std::fs::write(
            module.join("resources.crn"),
            r#"let target_group = awscc.elbv2.TargetGroup {
  name = "module"
}
"#,
        )
        .expect("module resources");

        let consumer_module = temp.path().join("consumer_module");
        std::fs::create_dir(&consumer_module).expect("consumer module directory");
        std::fs::write(
            consumer_module.join("main.crn"),
            r#"arguments {
  arn: String
}

attributes {
  arn = arn
}
"#,
        )
        .expect("consumer module");

        write_provider(&caller);
        std::fs::write(
            caller.join("module_call.crn"),
            r#"let component = use { source = '../module' }

let instance = component { }
"#,
        )
        .expect("caller module call");
        for (name, source) in caller_files {
            std::fs::write(caller.join(name), source).expect("caller fixture file");
        }

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

#[test]
fn unannotated_export_rejects_unknown_composition_attribute_once_with_suggestion() {
    let fixture = Fixture::direct(&[(
        "exports.crn",
        "exports {\n  x = instance.target_group_ar\n}\n",
    )]);

    let diags = fixture.validate();

    assert_eq!(
        diags.len(),
        1,
        "composition-backed Unknown inference must produce one existence diagnostic: {diags:#?}",
    );
    assert!(
        diags[0].contains("unknown attribute 'target_group_ar' on 'instance'"),
        "expected the missing composition attribute to be rejected: {diags:#?}",
    );
    assert!(
        diags[0].contains("Did you mean 'target_group_arn'?"),
        "expected a composition-attribute suggestion: {diags:#?}",
    );
}

#[test]
fn unannotated_export_accepts_declared_composition_attribute() {
    let fixture = Fixture::direct(&[(
        "exports.crn",
        "exports {\n  x = instance.target_group_arn\n}\n",
    )]);

    let diags = fixture.validate();

    assert!(
        diags.is_empty(),
        "declared composition attribute should pass: {diags:#?}"
    );
}

#[test]
fn managed_resource_rejects_unknown_composition_attribute() {
    let fixture = Fixture::direct(&[(
        "resources.crn",
        r#"let consumer = awscc.test.Consumer {
  name             = "caller"
  target_group_arn = instance.target_group_ar
}
"#,
    )]);

    let diags = fixture.validate();

    assert!(
        diags
            .iter()
            .any(|diag| diag.contains("unknown attribute 'target_group_ar' on 'instance'")),
        "expected the managed resource reference to be rejected: {diags:#?}",
    );
}

#[test]
fn managed_resource_accepts_declared_composition_attribute() {
    let fixture = Fixture::direct(&[(
        "resources.crn",
        r#"let consumer = awscc.test.Consumer {
  name             = "caller"
  target_group_arn = instance.target_group_arn
}
"#,
    )]);

    let diags = fixture.validate();

    assert!(
        diags.is_empty(),
        "declared composition attribute should pass: {diags:#?}"
    );
}

#[test]
fn module_call_argument_rejects_unknown_composition_attribute_once() {
    let fixture = Fixture::direct(&[(
        "zz_consumer_call.crn",
        r#"let consumer_component = use { source = '../consumer_module' }

let b = consumer_component {
  arn = instance.target_group_ar
}
"#,
    )]);

    let diags = fixture.validate();

    assert_eq!(
        diags.len(),
        1,
        "one bad module-call argument ref should produce exactly one diagnostic: {diags:#?}",
    );
    assert_eq!(
        diags[0],
        "module call 'b': unknown attribute 'target_group_ar' on 'instance' in reference instance.target_group_ar Did you mean 'target_group_arn'?",
    );
}

#[test]
fn module_call_argument_accepts_declared_composition_attribute() {
    let fixture = Fixture::direct(&[(
        "zz_consumer_call.crn",
        r#"let consumer_component = use { source = '../consumer_module' }

let b = consumer_component {
  arn = instance.target_group_arn
}
"#,
    )]);

    let diags = fixture.validate();

    assert!(
        diags.is_empty(),
        "declared composition attribute in a module-call argument should pass: {diags:#?}",
    );
}

#[test]
fn module_call_argument_rejects_unknown_schema_attribute() {
    let fixture = Fixture::direct(&[
        (
            "resources.crn",
            r#"let tg = awscc.elbv2.TargetGroup {
  name = "caller"
}
"#,
        ),
        (
            "zz_consumer_call.crn",
            r#"let consumer_component = use { source = '../consumer_module' }

let b = consumer_component {
  arn = tg.target_group_ar
}
"#,
        ),
    ]);

    let diags = fixture.validate();

    assert_eq!(
        diags.len(),
        1,
        "one schema-backed typo in a module-call argument should produce exactly one diagnostic: {diags:#?}",
    );
    assert_eq!(
        diags[0],
        "module call 'b': unknown attribute 'target_group_ar' on 'tg' in reference tg.target_group_ar Did you mean 'target_group_arn'?",
    );
}

#[test]
fn wait_rejects_composition_target_as_unsupported() {
    let fixture = Fixture::direct(&[(
        "wait.crn",
        r#"let ready = wait instance {
  until = instance.target_group_arn == "ready"
}
"#,
    )]);

    let diags = fixture.validate();

    assert!(
        diags.iter().any(|diag| {
            diag.contains("wait `ready`")
                && diag.contains("target binding `instance` is a composition")
                && diag.contains("not supported")
        }),
        "wait cannot poll a module-call composition and must fail during validation: {diags:#?}",
    );
}

#[test]
fn unannotated_export_bad_schema_attribute_reports_only_inference_diagnostic() {
    let fixture = Fixture::direct(&[
        (
            "resources.crn",
            r#"let tg = awscc.elbv2.TargetGroup {
  name = "caller"
}
"#,
        ),
        ("exports.crn", "exports {\n  x = tg.target_group_ar\n}\n"),
    ]);

    let diags = fixture.validate();

    assert_eq!(
        diags.len(),
        1,
        "schema-backed Unknown inference must not be duplicated by ref validation: {diags:#?}",
    );
    assert!(
        diags[0].contains("export 'x': type annotation required: unknown attribute"),
        "expected the inference diagnostic: {diags:#?}",
    );
    assert!(
        diags[0].contains("target_group_ar") && diags[0].contains("tg"),
        "inference diagnostic should identify the bad schema-backed reference: {diags:#?}",
    );
}

#[test]
fn unannotated_export_nested_schema_field_typo_reports_once() {
    let fixture = Fixture::direct(&[
        (
            "resources.crn",
            r#"let tg = awscc.elbv2.TargetGroup {
  name = "caller"
}
"#,
        ),
        ("exports.crn", "exports {\n  x = tg.tags.bad_field\n}\n"),
    ]);

    let diags = fixture.validate();

    assert_eq!(
        diags.len(),
        1,
        "retry inference should report the nested schema-field typo exactly once: {diags:#?}",
    );
    assert!(
        diags[0].contains("export 'x': type annotation required")
            && diags[0].contains("bad_field")
            && diags[0].contains("unknown attribute `tags.bad_field` on `tg`"),
        "the single diagnostic should identify the nested field typo: {diags:#?}",
    );
}

#[test]
fn duplicate_export_names_do_not_suppress_a_different_params_schema_typo() {
    let fixture = Fixture::direct(&[
        ("a_mixed.crn", "exports {\n  mixed = [\"text\", 1]\n}\n"),
        (
            "b_resource.crn",
            r#"let tg = awscc.elbv2.TargetGroup {
  name = "caller"
}
"#,
        ),
        ("c_typo.crn", "exports {\n  mixed = tg.target_group_ar\n}\n"),
    ]);

    let diags = fixture.validate();

    assert_eq!(
        diags.len(),
        3,
        "the duplicate declaration and both independent inference errors must be reported: {diags:#?}",
    );
    assert!(
        diags.iter().any(|diag| {
            diag.contains("duplicate export 'mixed'")
                && diag.contains("a_mixed.crn")
                && diag.contains("c_typo.crn")
        }),
        "expected the duplicate-export diagnostic with both source files: {diags:#?}",
    );
    assert!(
        diags
            .iter()
            .any(|diag| diag.contains("collection element types disagree")),
        "expected the heterogeneous-list inference diagnostic: {diags:#?}",
    );
    assert!(
        diags.iter().any(|diag| {
            diag.contains("unknown attribute")
                && diag.contains("target_group_ar")
                && diag.contains("tg")
        }),
        "expected the second export's schema-attribute typo: {diags:#?}",
    );
}

#[test]
fn unannotated_export_of_struct_with_nested_union_stays_compatibility_clean() {
    let fixture = Fixture::direct(&[
        (
            "resources.crn",
            r#"let policy = awscc.iam.Policy {
  name = "caller"
}
"#,
        ),
        (
            "exports.crn",
            "exports {\n  doc = policy.policy_document\n}\n",
        ),
    ]);

    let diags = fixture.validate();

    assert!(
        diags.is_empty(),
        "retry must not upgrade Unknown into a lossy inferred type that mismatches its source schema: {diags:#?}",
    );
}

#[test]
fn prior_heterogeneous_export_error_is_not_rederived_by_plan_style_validation() {
    let fixture = Fixture::direct(&[("exports.crn", "exports {\n  mixed = [\"text\", 1]\n}\n")]);

    let validate_diags = fixture.validate();
    assert_eq!(
        validate_diags.len(),
        1,
        "validate should report the original load-time inference error once: {validate_diags:#?}",
    );
    assert!(
        validate_diags[0].contains("type annotation required")
            && validate_diags[0].contains("collection element types disagree"),
        "expected the heterogeneous-list inference diagnostic: {validate_diags:#?}",
    );

    let loaded = carina_core::config_loader::load_configuration_with_config(
        &fixture.caller,
        &carina_core::parser::ProviderContext::default(),
        &carina_core::schema::SchemaRegistry::new(),
    )
    .expect("load plan-style fixture");
    assert_eq!(
        loaded.inference_errors.len(),
        1,
        "fixture must carry exactly the load-time inference error",
    );
    let inference_errors = loaded.inference_errors;
    let mut parsed = loaded.parsed;
    let retry_errors = carina_cli::commands::validate_and_resolve_errors_with_factories(
        &mut parsed,
        carina_core::config_loader::get_base_dir(&fixture.caller),
        false,
        factories(),
        HashMap::new(),
        &inference_errors,
        &[],
    );

    assert!(
        retry_errors.is_empty(),
        "plan/apply-style validation must not re-report a prior load-time inference error: {retry_errors:#?}",
    );
}

#[test]
fn nested_module_chain_accepts_outer_declared_attribute() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inner = temp.path().join("inner");
    let outer = temp.path().join("outer");
    let caller = temp.path().join("caller");
    std::fs::create_dir(&inner).expect("inner directory");
    std::fs::create_dir(&outer).expect("outer directory");
    std::fs::create_dir(&caller).expect("caller directory");

    std::fs::write(
        inner.join("attributes.crn"),
        "attributes {\n  target_group_arn = target_group.target_group_arn\n}\n",
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
        outer.join("module_call.crn"),
        r#"let inner_module = use { source = '../inner' }

let inner_instance = inner_module { }
"#,
    )
    .expect("outer module call");
    std::fs::write(
        outer.join("attributes.crn"),
        "attributes {\n  target_group_arn = inner_instance.target_group_arn\n}\n",
    )
    .expect("outer attributes");

    write_provider(&caller);
    std::fs::write(
        caller.join("module_call.crn"),
        r#"let outer_module = use { source = '../outer' }

let outer_instance = outer_module { }
"#,
    )
    .expect("caller module call");
    std::fs::write(
        caller.join("exports.crn"),
        "exports {\n  x = outer_instance.target_group_arn\n}\n",
    )
    .expect("caller exports");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert!(
        diags.is_empty(),
        "nested declared composition attribute should pass: {diags:#?}"
    );
}

#[test]
fn nested_module_attribute_rejects_unknown_inner_module_attribute() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inner = temp.path().join("inner");
    let outer = temp.path().join("outer");
    let caller = temp.path().join("caller");
    std::fs::create_dir(&inner).expect("inner directory");
    std::fs::create_dir(&outer).expect("outer directory");
    std::fs::create_dir(&caller).expect("caller directory");

    std::fs::write(
        inner.join("attributes.crn"),
        "attributes {\n  target_group_arn = target_group.target_group_arn\n}\n",
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
        outer.join("module_call.crn"),
        r#"let inner_module = use { source = '../inner' }

let inner_instance = inner_module { }
"#,
    )
    .expect("outer module call");
    std::fs::write(
        outer.join("attributes.crn"),
        "attributes {\n  target_group_arn = inner_instance.target_group_ar\n}\n",
    )
    .expect("outer attributes");

    write_provider(&caller);
    std::fs::write(
        caller.join("module_call.crn"),
        r#"let outer_module = use { source = '../outer' }

let outer_instance = outer_module { }
"#,
    )
    .expect("caller module call");

    let diags = carina_cli::commands::validate::validate_with_factories(&caller, factories());

    assert_eq!(
        diags.len(),
        1,
        "one bad nested-module output reference should produce exactly one diagnostic: {diags:#?}",
    );
    assert!(
        diags[0].contains("../outer:")
            && diags[0]
                .contains("attribute 'target_group_arn': unknown attribute 'target_group_ar'")
            && diags[0].contains("inner_instance.target_group_ar")
            && diags[0].contains("Did you mean 'target_group_arn'?"),
        "diagnostic should identify the outer module path and inner output typo: {diags:#?}",
    );
}
