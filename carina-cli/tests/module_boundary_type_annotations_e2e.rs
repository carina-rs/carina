//! End-to-end regressions for carina#3798.
//!
//! Every fixture is directory-scoped and splits providers from resources so
//! module-boundary type checking exercises the production loader and module
//! expansion paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{DataSource, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, TypeIdentity};
use indexmap::IndexMap;
use tempfile::TempDir;

struct AwsTestFactory;

impl ProviderFactory for AwsTestFactory {
    fn name(&self) -> &str {
        "aws"
    }

    fn display_name(&self) -> &str {
        "AWS (carina#3798 validation stub)"
    }

    fn provider_config_attribute_types(&self) -> HashMap<String, AttributeType> {
        HashMap::new()
    }

    fn validate_config(&self, _attributes: &IndexMap<String, Value>) -> Result<(), String> {
        Ok(())
    }

    fn validate_custom_type(&self, _type_name: &TypeIdentity, _value: &str) -> Result<(), String> {
        Ok(())
    }

    fn extract_region(&self, _attributes: &IndexMap<String, Value>) -> String {
        "ap-northeast-1".to_string()
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
        vec![vpc_schema(), subnet_schema(), security_group_schema()]
    }
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

fn identity(path: &str, kind: &str) -> AttributeType {
    AttributeType::refined_string(
        Some(TypeIdentity::from_schema_type("aws", path, kind)),
        None,
        None,
        None,
    )
}

fn vpc_schema() -> ResourceSchema {
    ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::new("name", AttributeType::string()).required())
        .attribute(AttributeSchema::new("vpc_id", identity("ec2.Vpc", "Id")).read_only())
        .with_unique_name_attribute("name")
}

fn subnet_schema() -> ResourceSchema {
    ResourceSchema::new("ec2.Subnet")
        .attribute(AttributeSchema::new("name", AttributeType::string()).required())
        .attribute(AttributeSchema::new("subnet_id", identity("ec2.Subnet", "Id")).read_only())
        .with_unique_name_attribute("name")
}

fn security_group_schema() -> ResourceSchema {
    ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::new("name", AttributeType::string()).required())
        .attribute(AttributeSchema::new("vpc_id", identity("ec2.Vpc", "Id")).required())
        .attribute(
            AttributeSchema::new("group_id", identity("ec2.SecurityGroup", "Id")).read_only(),
        )
        .with_unique_name_attribute("name")
}

fn factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(AwsTestFactory) as Box<dyn ProviderFactory>]
}

struct Fixture {
    _temp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn module(module_files: &[(&str, &str)], root_files: &[(&str, &str)]) -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let module = temp.path().join("web_tier");
        let root = temp.path().join("root");
        std::fs::create_dir(&module).expect("module directory");
        std::fs::create_dir(&root).expect("root directory");
        for (name, source) in module_files {
            std::fs::write(module.join(name), source).expect("module fixture file");
        }
        write_provider(&root);
        for (name, source) in root_files {
            std::fs::write(root.join(name), source).expect("root fixture file");
        }
        Self { _temp: temp, root }
    }

    fn validate(&self) -> Vec<String> {
        carina_cli::commands::validate::validate_with_factories(&self.root, factories())
    }
}

fn write_provider(dir: &Path) {
    std::fs::write(
        dir.join("providers.crn"),
        r#"provider aws {
  region = "ap-northeast-1"
}
"#,
    )
    .expect("provider fixture");
}

const MODULE_SECURITY_GROUP: &str = r#"let web_sg = aws.ec2.SecurityGroup {
  name   = "web"
  vpc_id = vpc_id
}
"#;

fn issue_module(argument_type: &str, attribute_declaration: &str) -> [(&'static str, String); 3] {
    [
        (
            "arguments.crn",
            format!("arguments {{\n  vpc_id: {argument_type}\n}}\n"),
        ),
        (
            "attributes.crn",
            format!(
                "attributes {{\n  security_group_id{attribute_declaration} = web_sg.group_id\n}}\n"
            ),
        ),
        ("resources.crn", MODULE_SECURITY_GROUP.to_string()),
    ]
}

fn borrowed<'a>(files: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    files
        .iter()
        .map(|(name, source)| (*name, source.as_str()))
        .collect()
}

fn root_with_two_calls() -> &'static str {
    r#"let web_tier = use { source = '../web_tier' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

let web = web_tier {
  vpc_id = main_vpc.vpc_id
}

let web2 = web_tier {
  vpc_id = web.security_group_id
}
"#
}

#[test]
fn schema_typed_module_argument_rejects_composition_attribute_at_call_boundary() {
    let module = issue_module("aws.ec2.Vpc.Id", ": aws.ec2.SecurityGroup.Id");
    let module = borrowed(&module);
    let fixture = Fixture::module(&module, &[("main.crn", root_with_two_calls())]);

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("module call 'web2': argument 'vpc_id'")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from web.security_group_id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        1,
        "the root call boundary must report its identity mismatch exactly once: {diagnostics:#?}",
    );
}

#[test]
fn schema_typed_list_argument_checks_each_nested_reference() {
    let fixture = Fixture::module(
        &[(
            "arguments.crn",
            "arguments {\n  subnet_ids: list(aws.ec2.Subnet.Id)\n}\n",
        )],
        &[(
            "main.crn",
            r#"let consumer = use { source = '../web_tier' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

let a = aws.ec2.Subnet {
  name = "a"
}

let web = aws.ec2.SecurityGroup {
  name   = "web"
  vpc_id = main_vpc.vpc_id
}

let instance = consumer {
  subnet_ids = [a.subnet_id, web.group_id]
}
"#,
        )],
    );

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("module call 'instance': argument 'subnet_ids'")
                && diagnostic.contains("expected aws.ec2.Subnet.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from web.group_id")
        }),
        "each list element must be checked against the element sink: {diagnostics:#?}",
    );
}

#[test]
fn ref_typed_module_argument_stays_unchecked_but_inner_resource_rejects_composition_type() {
    let module = issue_module("aws.ec2.Vpc", "");
    let module = borrowed(&module);
    let fixture = Fixture::module(&module, &[("main.crn", root_with_two_calls())]);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("aws.ec2.SecurityGroup.web2.web_sg")
                && diagnostic.contains("cannot assign aws.ec2.SecurityGroup.Id to 'vpc_id'")
                && diagnostic.contains("from web.security_group_id")
        }),
        "expected the expanded inner resource to reject the inferred type, got: {diagnostics:#?}",
    );
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.contains("module call 'web2': argument 'vpc_id'")),
        "Ref annotations remain unchecked until carina#3803: {diagnostics:#?}",
    );
}

fn assert_plain_resource_consumer_rejected(attribute_declaration: &str) {
    let module = issue_module("aws.ec2.Vpc.Id", attribute_declaration);
    let module = borrowed(&module);
    let fixture = Fixture::module(
        &module,
        &[(
            "main.crn",
            r#"let web_tier = use { source = '../web_tier' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

let web = web_tier {
  vpc_id = main_vpc.vpc_id
}

let consumer = aws.ec2.SecurityGroup {
  name   = "consumer"
  vpc_id = web.security_group_id
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("aws.ec2.SecurityGroup.consumer")
                && diagnostic.contains("cannot assign")
                && diagnostic.contains("to 'vpc_id'")
                && diagnostic.contains("from web.security_group_id")
        }),
        "expected composition consumer mismatch for {attribute_declaration:?}, got: {diagnostics:#?}",
    );
}

#[test]
fn unannotated_composition_attribute_is_inferred_for_resource_consumer() {
    assert_plain_resource_consumer_rejected("");
}

#[test]
fn string_annotated_composition_attribute_is_too_wide_for_identity_sink() {
    assert_plain_resource_consumer_rejected(": String");
}

#[test]
fn declared_string_output_overrides_forwarded_vpc_id_inference_after_expansion() {
    let diagnostics = vpc_output_fixture(": String").validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("aws.ec2.SecurityGroup.consumer")
                && diagnostic.contains("cannot assign String to 'vpc_id'")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("from web.vpc_id")
        }),
        "the expanded composition must carry the declared String type instead of the forwarded Vpc.Id inference: {diagnostics:#?}",
    );
}

#[test]
fn security_group_id_annotated_composition_attribute_rejects_vpc_id_sink() {
    assert_plain_resource_consumer_rejected(": aws.ec2.SecurityGroup.Id");
}

#[test]
fn attribute_declaration_rejects_security_group_id_as_vpc_id() {
    let module = issue_module("aws.ec2.Vpc.Id", ": aws.ec2.Vpc.Id");
    let module = borrowed(&module);
    let fixture = Fixture::module(&module, &[("main.crn", root_with_two_calls())]);

    let diagnostics = fixture.validate();

    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("attribute 'security_group_id': type mismatch")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from web_sg.group_id")
        }),
        "expected the declaration mismatch, got: {diagnostics:#?}",
    );
}

fn vpc_output_fixture(attribute_declaration: &str) -> Fixture {
    Fixture::module(
        &[
            (
                "attributes.crn",
                &format!(
                    "attributes {{\n  vpc_id{attribute_declaration} = module_vpc.vpc_id\n}}\n"
                ),
            ),
            (
                "resources.crn",
                r#"let module_vpc = aws.ec2.Vpc {
  name = "module"
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let vpc_module = use { source = '../web_tier' }

let web = vpc_module { }

let consumer = aws.ec2.SecurityGroup {
  name   = "consumer"
  vpc_id = web.vpc_id
}
"#,
        )],
    )
}

#[test]
fn matching_schema_typed_composition_attribute_passes() {
    let diagnostics = vpc_output_fixture(": aws.ec2.Vpc.Id").validate();
    assert!(
        diagnostics.is_empty(),
        "expected valid identity flow: {diagnostics:#?}"
    );
}

#[test]
fn unannotated_vpc_id_forward_is_inferred_and_passes() {
    let diagnostics = vpc_output_fixture("").validate();
    assert!(
        diagnostics.is_empty(),
        "expected valid inferred flow: {diagnostics:#?}"
    );
}

#[test]
fn ref_annotated_composition_attribute_infers_forwarded_identity() {
    let diagnostics = vpc_output_fixture(": aws.ec2.Vpc").validate();
    assert!(
        diagnostics.is_empty(),
        "Ref is unchecked while its forwarded value remains inferable: {diagnostics:#?}",
    );
}

#[test]
fn nested_composition_forward_chain_preserves_inferred_identity() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inner = temp.path().join("inner");
    let outer = temp.path().join("outer");
    let root = temp.path().join("root");
    std::fs::create_dir(&inner).expect("inner directory");
    std::fs::create_dir(&outer).expect("outer directory");
    std::fs::create_dir(&root).expect("root directory");

    std::fs::write(
        inner.join("main.crn"),
        r#"attributes {
  vpc_id = inner_vpc.vpc_id
}

let inner_vpc = aws.ec2.Vpc {
  name = "inner"
}
"#,
    )
    .expect("inner module");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner_module = use { source = '../inner' }

let inner_instance = inner_module { }

attributes {
  vpc_id = inner_instance.vpc_id
}
"#,
    )
    .expect("outer module");
    write_provider(&root);
    std::fs::write(
        root.join("main.crn"),
        r#"let outer_module = use { source = '../outer' }

let outer_instance = outer_module { }

let consumer = aws.ec2.SecurityGroup {
  name   = "consumer"
  vpc_id = outer_instance.vpc_id
}
"#,
    )
    .expect("root module");
    let fixture = Fixture { _temp: temp, root };

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.is_empty(),
        "expected nested composition inference to preserve Vpc.Id: {diagnostics:#?}",
    );
}

#[test]
fn nested_module_call_checks_declared_output_type_at_its_call_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let web_tier = temp.path().join("web_tier");
    let needs_vpc = temp.path().join("needs_vpc");
    let outer = temp.path().join("outer");
    let root = temp.path().join("root");
    for directory in [&web_tier, &needs_vpc, &outer, &root] {
        std::fs::create_dir(directory).expect("fixture directory");
    }

    std::fs::write(
        web_tier.join("main.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let web_sg = aws.ec2.SecurityGroup {
  name   = "web"
  vpc_id = vpc_id
}

attributes {
  sg_id: aws.ec2.SecurityGroup.Id = web_sg.group_id
}
"#,
    )
    .expect("web_tier module");
    std::fs::write(
        needs_vpc.join("main.crn"),
        "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
    )
    .expect("needs_vpc module");
    std::fs::write(
        outer.join("main.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let web_tier = use { source = '../web_tier' }
let needs_vpc = use { source = '../needs_vpc' }

let a = web_tier {
  vpc_id = vpc_id
}

let b = needs_vpc {
  vpc_id = a.sg_id
}
"#,
    )
    .expect("outer module");
    write_provider(&root);
    std::fs::write(
        root.join("main.crn"),
        r#"let outer = use { source = '../outer' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

let instance = outer {
  vpc_id = main_vpc.vpc_id
}
"#,
    )
    .expect("root module");
    let fixture = Fixture { _temp: temp, root };

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("module call 'b': argument 'vpc_id'")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from a.sg_id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        1,
        "every nested call boundary must be checked exactly once: {diagnostics:#?}",
    );
}

#[test]
fn schema_typed_export_rejects_wrong_identity_source() {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("root");
    std::fs::create_dir(&root).expect("root directory");
    write_provider(&root);
    std::fs::write(
        root.join("main.crn"),
        r#"let vpc = aws.ec2.Vpc {
  name = "main"
}

let sg = aws.ec2.SecurityGroup {
  name   = "sg"
  vpc_id = vpc.vpc_id
}
"#,
    )
    .expect("root resources");
    std::fs::write(
        root.join("exports.crn"),
        r#"exports {
  bad: aws.ec2.Vpc.Id = sg.group_id
}
"#,
    )
    .expect("root exports");
    let fixture = Fixture { _temp: temp, root };

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("export 'bad': type mismatch")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
        }),
        "expected export identity mismatch, got: {diagnostics:#?}",
    );
}
