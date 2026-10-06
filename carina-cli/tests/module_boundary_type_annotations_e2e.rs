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
use carina_core::resource::Value;
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
};
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
    ) -> BoxFuture<'_, ProviderResult<Box<dyn ProviderNormalizer>>> {
        Box::pin(async { Ok(Box::new(NoopNormalizer) as Box<dyn ProviderNormalizer>) })
    }

    fn schemas(&self) -> Vec<ResourceSchema> {
        vec![
            vpc_schema(),
            subnet_schema(),
            security_group_schema(),
            log_group_schema(),
            hosted_zone_schema(),
            domain_lookup_schema(),
            nested_domain_consumer_schema(),
        ]
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
        resource: &carina_core::provider::ProviderReadyDataSource,
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
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
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

fn log_group_schema() -> ResourceSchema {
    ResourceSchema::new("logs.LogGroup")
        .attribute(AttributeSchema::new("name", AttributeType::string()).required())
        .attribute(AttributeSchema::new("arn", identity("logs.LogGroup", "Arn")).read_only())
        .with_unique_name_attribute("name")
}

fn hosted_zone_schema() -> ResourceSchema {
    let domain_name = AttributeType::refined_string(None, None, Some((None, Some(1024))), None);
    ResourceSchema::new("route53.HostedZone")
        .attribute(AttributeSchema::new("name", domain_name).required())
        .with_unique_name_attribute("name")
}

fn domain_lookup_schema() -> ResourceSchema {
    ResourceSchema::new("test.DomainLookup")
        .attribute(AttributeSchema::new("query", AttributeType::string()).required())
        .attribute(AttributeSchema::new("domain_name", AttributeType::string()).read_only())
        .as_data_source()
}

fn nested_domain_consumer_schema() -> ResourceSchema {
    let constrained_domain = AttributeType::refined_string(
        None,
        Some(r"^[A-Za-z0-9.-]+$".to_string()),
        Some((Some(1), Some(1024))),
        None,
    );
    let endpoint = AttributeType::struct_(
        "Endpoint",
        vec![StructField::new("domain_name", constrained_domain)],
    );
    ResourceSchema::new("test.NestedDomainConsumer")
        .attribute(AttributeSchema::new("name", AttributeType::string()).required())
        .attribute(AttributeSchema::new("endpoint", endpoint).required())
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
        Self::module_with_upstream(module_files, root_files, &[])
    }

    fn module_with_upstream(
        module_files: &[(&str, &str)],
        root_files: &[(&str, &str)],
        upstream_files: &[(&str, &str)],
    ) -> Self {
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
        if !upstream_files.is_empty() {
            let upstream = temp.path().join("upstream");
            std::fs::create_dir(&upstream).expect("upstream directory");
            for (name, source) in upstream_files {
                std::fs::write(upstream.join(name), source).expect("upstream fixture file");
            }
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
fn module_output_dedup_keeps_each_distinct_error_once_across_three_instances() {
    let fixture = Fixture::module(
        &[
            ("arguments.crn", "arguments {\n  n: String\n}\n"),
            (
                "attributes.crn",
                r#"attributes {
  both: list(Bool) = [src.group_id, src.nonexistent_attr]
}
"#,
            ),
            (
                "resources.crn",
                r#"let src = aws.ec2.SecurityGroup {
  name   = n
  vpc_id = "vpc-fixed"
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let bad_module = use { source = '../web_tier' }

let one = bad_module { n = "one" }
let two = bad_module { n = "two" }
let three = bad_module { n = "three" }
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let declaration_errors: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("attribute 'both'"))
        .collect();

    assert_eq!(
        declaration_errors.len(),
        2,
        "both authored failures must survive, with instance copies deduplicated: {diagnostics:#?}",
    );
    assert_eq!(
        declaration_errors
            .iter()
            .filter(|diagnostic| diagnostic.contains("type mismatch"))
            .count(),
        1,
        "the mismatch must be reported once: {diagnostics:#?}",
    );
    assert_eq!(
        declaration_errors
            .iter()
            .filter(|diagnostic| diagnostic.contains("unknown attribute 'nonexistent_attr'"))
            .count(),
        1,
        "the unknown attribute must be reported once: {diagnostics:#?}",
    );
}

#[test]
fn module_output_dedup_keeps_two_same_kind_reference_positions() {
    let fixture = Fixture::module(
        &[
            ("arguments.crn", "arguments {\n  n: String\n}\n"),
            (
                "attributes.crn",
                r#"attributes {
  y: list(Bool) = [p.group_id, q.group_id]
}
"#,
            ),
            (
                "resources.crn",
                r#"let p = aws.ec2.SecurityGroup {
  name   = n
  vpc_id = "vpc-fixed"
}

let q = aws.ec2.SecurityGroup {
  name   = "q"
  vpc_id = "vpc-fixed"
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let bad_module = use { source = '../web_tier' }

let one = bad_module { n = "one" }
let two = bad_module { n = "two" }
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("attribute 'y': type mismatch")
                && diagnostic.contains("expected Bool")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        2,
        "both authored references with the same error kind must survive instance dedup: {diagnostics:#?}",
    );
    assert!(mismatches.iter().any(|error| error.contains("p.group_id")));
    assert!(mismatches.iter().any(|error| error.contains("q.group_id")));
}

#[test]
fn module_call_dedup_keeps_two_same_kind_reference_positions() {
    let fixture = Fixture::module(
        &[
            (
                "arguments.crn",
                "arguments {\n  vpc_ids: list(aws.ec2.Vpc.Id)\n}\n",
            ),
            (
                "attributes.crn",
                r#"attributes {
  sg: aws.ec2.SecurityGroup.Id = s.group_id
}
"#,
            ),
            (
                "resources.crn",
                r#"let s = aws.ec2.SecurityGroup {
  name   = "module"
  vpc_id = "vpc-fixed"
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }

let v = aws.ec2.Vpc { name = "v" }
let a = m { vpc_ids = [v.vpc_id] }
let c = m { vpc_ids = [v.vpc_id] }
let b = m { vpc_ids = [a.sg, c.sg] }
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("module call 'b': argument 'vpc_ids'")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        2,
        "both argument references with the same error kind must survive dedup: {diagnostics:#?}",
    );
    assert!(mismatches.iter().any(|error| error.contains("a.sg")));
    assert!(mismatches.iter().any(|error| error.contains("c.sg")));
}

#[test]
fn module_attribute_checks_bare_string_argument_as_typed_source() {
    let fixture = Fixture::module(
        &[
            ("arguments.crn", "arguments {\n  vpc_id: String\n}\n"),
            (
                "attributes.crn",
                "attributes {\n  x: aws.ec2.Vpc.Id = vpc_id\n}\n",
            ),
        ],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let v = aws.ec2.Vpc { name = "v" }
let instance = m { vpc_id = v.vpc_id }
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("attribute 'x': type mismatch"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{diagnostics:#?}");
    assert!(mismatches[0].contains("expected aws.ec2.Vpc.Id"));
    assert!(mismatches[0].contains("got String"));
    assert!(mismatches[0].contains("from vpc_id"));
}

#[test]
fn module_resource_checks_bare_string_argument_as_typed_source() {
    let fixture = Fixture::module(
        &[
            ("arguments.crn", "arguments {\n  vpc_id: String\n}\n"),
            (
                "resources.crn",
                r#"let s = aws.ec2.SecurityGroup {
  name   = "module"
  vpc_id = vpc_id
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let v = aws.ec2.Vpc { name = "v" }
let instance = m { vpc_id = v.vpc_id }
"#,
        )],
    );

    let diagnostics = fixture.validate();
    // Issue #3798 case (c): rule 10 remains strict when the sink carries the
    // aws.ec2.Vpc.Id identity; the module argument has no identity evidence.
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("cannot assign String to 'vpc_id'")
                && diagnostic.contains("from vpc_id")
        })
        .collect();
    assert_eq!(mismatches.len(), 1, "{diagnostics:#?}");
    assert!(mismatches[0].contains("../web_tier"));
}

#[test]
fn module_string_argument_flows_to_awscc_like_identityless_length_sink() {
    let fixture = Fixture::module(
        &[
            ("arguments.crn", "arguments {\n  domain_name: String\n}\n"),
            (
                "resources.crn",
                r#"let zone = aws.route53.HostedZone {
  name = domain_name
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let hosted_zone = use { source = '../web_tier' }

let zone = hosted_zone {
  domain_name = "example.com"
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.is_empty(),
        "plain module String should reach HostedZone.name's identity-less ..=1024 constraint: {diagnostics:#?}",
    );
}

#[test]
fn declared_string_output_preserves_forwarded_log_group_arn_evidence() {
    let fixture = Fixture::module(
        &[
            ("attributes.crn", "attributes {\n  x: String = lg.arn\n}\n"),
            (
                "resources.crn",
                r#"let lg = aws.logs.LogGroup {
  name = "module"
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let component = use { source = '../web_tier' }
let m = component { }

let zone = aws.route53.HostedZone {
  name = m.x
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("cannot assign aws.logs.LogGroup.Arn to 'name'")
                && diagnostic.contains("got aws.logs.LogGroup.Arn")
                && diagnostic.contains("from m.x, declared String")
        }),
        "the declared String contract must not erase the forwarded Arn evidence: {diagnostics:#?}",
    );
}

#[test]
fn declared_string_output_accepts_forwarded_plain_string_evidence() {
    let fixture = Fixture::module(
        &[
            (
                "attributes.crn",
                "attributes {\n  x: String = lookup.domain_name\n}\n",
            ),
            (
                "resources.crn",
                r#"let lookup = read aws.test.DomainLookup {
  query = "example.com"
}
"#,
            ),
        ],
        &[(
            "main.crn",
            r#"let component = use { source = '../web_tier' }
let m = component { }

let zone = aws.route53.HostedZone {
  name = m.x
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.is_empty(),
        "rule 10 must still accept declared and forwarded plain String evidence: {diagnostics:#?}",
    );
}

#[test]
fn nested_declared_string_outputs_accumulate_forwarded_evidence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let inner = temp.path().join("inner");
    let outer = temp.path().join("outer");
    let root = temp.path().join("root");
    std::fs::create_dir(&inner).expect("inner directory");
    std::fs::create_dir(&outer).expect("outer directory");
    std::fs::create_dir(&root).expect("root directory");

    std::fs::write(
        inner.join("attributes.crn"),
        "attributes {\n  x: String = lg.arn\n}\n",
    )
    .expect("inner attributes");
    std::fs::write(
        inner.join("resources.crn"),
        r#"let lg = aws.logs.LogGroup {
  name = "inner"
}
"#,
    )
    .expect("inner resources");

    std::fs::write(
        outer.join("module.crn"),
        r#"let inner_component = use { source = '../inner' }
let inner_instance = inner_component { }
"#,
    )
    .expect("outer module call");
    std::fs::write(
        outer.join("attributes.crn"),
        "attributes {\n  x: String = inner_instance.x\n}\n",
    )
    .expect("outer attributes");

    write_provider(&root);
    std::fs::write(
        root.join("main.crn"),
        r#"let outer_component = use { source = '../outer' }
let outer_instance = outer_component { }

let zone = aws.route53.HostedZone {
  name = outer_instance.x
}
"#,
    )
    .expect("root resources");

    let diagnostics = carina_cli::commands::validate::validate_with_factories(&root, factories());
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("cannot assign aws.logs.LogGroup.Arn to 'name'")
                && diagnostic.contains("got aws.logs.LogGroup.Arn")
                && diagnostic.contains("from outer_instance.x, declared String")
        }),
        "both declared String boundaries must retain the original Arn evidence: {diagnostics:#?}",
    );
}

#[test]
fn data_source_string_attribute_flows_to_nested_identityless_refinement() {
    let fixture = Fixture::module(
        &[],
        &[(
            "main.crn",
            r#"let lookup = read aws.test.DomainLookup {
  query = "example.com"
}

let consumer = aws.test.NestedDomainConsumer {
  name = "consumer"
  endpoint = {
    domain_name = lookup.domain_name
  }
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.is_empty(),
        "a data-source String should recurse into a nested pattern+length sink: {diagnostics:#?}",
    );
}

#[test]
fn module_attribute_checks_struct_argument_field_as_typed_source() {
    let fixture = Fixture::module(
        &[
            (
                "arguments.crn",
                "arguments {\n  cfg: struct { vpc: aws.ec2.SecurityGroup.Id }\n}\n",
            ),
            (
                "attributes.crn",
                "attributes {\n  x: aws.ec2.Vpc.Id = cfg.vpc\n}\n",
            ),
        ],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let v = aws.ec2.Vpc { name = "v" }
let sg = aws.ec2.SecurityGroup {
  name   = "sg"
  vpc_id = v.vpc_id
}
let instance = m { cfg = { vpc = sg.group_id } }
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("attribute 'x': type mismatch"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{diagnostics:#?}");
    assert!(mismatches[0].contains("expected aws.ec2.Vpc.Id"));
    assert!(mismatches[0].contains("got aws.ec2.SecurityGroup.Id"));
    assert!(mismatches[0].contains("from cfg.vpc"));
}

#[test]
fn unannotated_forwarded_output_keeps_module_argument_type_after_expansion() {
    let fixture = Fixture::module(
        &[
            ("arguments.crn", "arguments {\n  value: String\n}\n"),
            ("attributes.crn", "attributes {\n  forwarded = value\n}\n"),
        ],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let v = aws.ec2.Vpc { name = "v" }
let instance = m { value = v.vpc_id }
let consumer = aws.ec2.SecurityGroup {
  name   = "consumer"
  vpc_id = instance.forwarded
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("aws.ec2.SecurityGroup.consumer")
                && diagnostic.contains("cannot assign String to 'vpc_id'")
                && diagnostic.contains("from instance.forwarded")
        }),
        "the output must retain the module argument's declared String type: {diagnostics:#?}",
    );
}

#[test]
fn forwarded_argument_typo_is_reported_only_at_call_boundary() {
    let fixture = Fixture::module(
        &[
            (
                "arguments.crn",
                "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
            ),
            ("attributes.crn", "attributes {\n  x = vpc_id\n}\n"),
        ],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let v = aws.ec2.Vpc { name = "v" }
let a = m { vpc_id = v.vpc_id }
let b = m { vpc_id = a.nope }
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let typo_diagnostics: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("unknown attribute 'nope'"))
        .collect();
    assert_eq!(typo_diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(typo_diagnostics[0].contains("module call 'b'"));
    assert!(!typo_diagnostics[0].contains("attribute 'x'"));
}

#[test]
fn upstream_identity_is_checked_at_module_call_boundary() {
    let fixture = Fixture::module_with_upstream(
        &[(
            "arguments.crn",
            "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
        )],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let up = upstream_state { source = '../upstream' }
let bad = m { vpc_id = up.sg }
let good = m { vpc_id = up.vpc }
"#,
        )],
        &[(
            "exports.crn",
            r#"exports {
  sg: aws.ec2.SecurityGroup.Id = "sg-123"
  vpc: aws.ec2.Vpc.Id = "vpc-123"
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("module call 'bad': argument 'vpc_id'"))
        .collect();
    assert_eq!(mismatches.len(), 1, "{diagnostics:#?}");
    assert!(mismatches[0].contains("expected aws.ec2.Vpc.Id"));
    assert!(mismatches[0].contains("got aws.ec2.SecurityGroup.Id"));
    assert!(mismatches[0].contains("from up.sg"));
}

#[test]
fn upstream_security_group_id_is_rejected_by_resource_vpc_id_sink() {
    let fixture = Fixture::module_with_upstream(
        &[],
        &[(
            "main.crn",
            r#"let up = upstream_state { source = '../upstream' }
let bad = aws.ec2.SecurityGroup {
  name   = "bad"
  vpc_id = up.sg
}
"#,
        )],
        &[(
            "exports.crn",
            "exports {\n  sg: aws.ec2.SecurityGroup.Id = \"sg-123\"\n}\n",
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("aws.ec2.SecurityGroup.bad")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from up.sg")
        }),
        "SecurityGroup.Id must not flow into a Vpc.Id resource sink: {diagnostics:#?}",
    );
}

#[test]
fn unannotated_upstream_export_remains_unchecked_at_module_call_boundary() {
    let fixture = Fixture::module_with_upstream(
        &[(
            "arguments.crn",
            "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
        )],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let up = upstream_state { source = '../upstream' }
let instance = m { vpc_id = up.opaque }
"#,
        )],
        &[("exports.crn", "exports {\n  opaque = \"not-a-vpc-id\"\n}\n")],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| !diagnostic.contains("argument 'vpc_id'")),
        "an inferred type must not replace an explicit upstream contract: {diagnostics:#?}",
    );
}

#[test]
fn upstream_identity_is_checked_in_attribute_and_export_declarations() {
    let attribute_fixture = Fixture::module_with_upstream(
        &[],
        &[(
            "main.crn",
            r#"let up = upstream_state { source = '../upstream' }
attributes {
  attr: aws.ec2.Vpc.Id = up.sg
}
"#,
        )],
        &[(
            "exports.crn",
            "exports {\n  sg: aws.ec2.SecurityGroup.Id = \"sg-123\"\n}\n",
        )],
    );

    let diagnostics = attribute_fixture.validate();
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.contains("attribute 'attr': type mismatch"))
            .count(),
        1,
        "{diagnostics:#?}",
    );

    let export_fixture = Fixture::module_with_upstream(
        &[],
        &[(
            "main.crn",
            r#"let up = upstream_state { source = '../upstream' }
exports {
  exported: aws.ec2.Vpc.Id = up.sg
}
"#,
        )],
        &[(
            "exports.crn",
            "exports {\n  sg: aws.ec2.SecurityGroup.Id = \"sg-123\"\n}\n",
        )],
    );
    let diagnostics = export_fixture.validate();
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.contains("export 'exported': type mismatch"))
            .count(),
        1,
        "{diagnostics:#?}",
    );
}

#[test]
fn missing_upstream_export_in_module_argument_is_reported_once() {
    let fixture = Fixture::module_with_upstream(
        &[(
            "arguments.crn",
            "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
        )],
        &[(
            "main.crn",
            r#"let m = use { source = '../web_tier' }
let up = upstream_state { source = '../upstream' }
let bad = m { vpc_id = up.nope }
"#,
        )],
        &[(
            "exports.crn",
            "exports {\n  vpc: aws.ec2.Vpc.Id = \"vpc-123\"\n}\n",
        )],
    );

    let diagnostics = fixture.validate();
    let missing: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("does not export `nope`"))
        .collect();
    assert_eq!(missing.len(), 1, "{diagnostics:#?}");
}

#[test]
fn missing_field_in_typed_upstream_struct_is_reported_once() {
    let fixture = Fixture::module_with_upstream(
        &[],
        &[(
            "main.crn",
            r#"let up = upstream_state { source = '../upstream' }
let bad = aws.ec2.SecurityGroup {
  name = up.account.nope
}
"#,
        )],
        &[(
            "exports.crn",
            r#"exports {
  account: struct { id: String } = { id = "account-123" }
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    let missing: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.contains("nope"))
        .collect();
    assert_eq!(missing.len(), 1, "{diagnostics:#?}");
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
fn unannotated_composition_argument_is_inferred_at_root_call_boundary_once() {
    let module = issue_module("aws.ec2.Vpc.Id", "");
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
        "the post-expansion root call check must infer the output and report it once: {diagnostics:#?}",
    );
}

#[test]
fn anonymous_module_call_argument_mismatch_is_rejected_at_call_boundary() {
    let fixture = Fixture::module(
        &[
            (
                "arguments.crn",
                "arguments {\n  vpc_id: aws.ec2.Vpc.Id\n}\n",
            ),
            ("resources.crn", MODULE_SECURITY_GROUP),
        ],
        &[(
            "main.crn",
            r#"let web_tier = use { source = '../web_tier' }

let main_vpc = aws.ec2.Vpc {
  name = "main"
}

let wrong = aws.ec2.SecurityGroup {
  name   = "wrong"
  vpc_id = main_vpc.vpc_id
}

web_tier {
  vpc_id = wrong.group_id
}
"#,
        )],
    );

    let diagnostics = fixture.validate();
    assert!(
        diagnostics.iter().any(|diagnostic| {
            diagnostic.contains("module call 'web_tier (anonymous call)'")
                && diagnostic.contains("argument 'vpc_id'")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from wrong.group_id")
        }),
        "the anonymous composition must retain and validate its typed call boundary: {diagnostics:#?}",
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

    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("attribute 'security_group_id': type mismatch")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
        })
        .collect();
    assert_eq!(
        mismatches.len(),
        1,
        "one authored output declaration should report once regardless of call count: {diagnostics:#?}",
    );
    assert!(
        mismatches[0].contains("module '../web_tier'")
            && mismatches[0].contains("from web.web_sg.group_id"),
        "the declaration diagnostic should identify its module and one source path: {diagnostics:#?}",
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

fn nested_module_call_fixture(attribute_declaration: &str) -> Fixture {
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
        format!(
            r#"arguments {{
  vpc_id: aws.ec2.Vpc.Id
}}

let web_sg = aws.ec2.SecurityGroup {{
  name   = "web"
  vpc_id = vpc_id
}}

attributes {{
  sg_id{attribute_declaration} = web_sg.group_id
}}
"#
        ),
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
    Fixture { _temp: temp, root }
}

fn assert_nested_module_call_mismatch(attribute_declaration: &str) {
    let fixture = nested_module_call_fixture(attribute_declaration);

    let diagnostics = fixture.validate();
    let mismatches: Vec<_> = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("module call 'instance.b': argument 'vpc_id'")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from instance.a.sg_id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        1,
        "every nested call boundary must be checked exactly once: {diagnostics:#?}",
    );
}

#[test]
fn nested_module_call_checks_declared_output_type_at_its_call_boundary() {
    assert_nested_module_call_mismatch(": aws.ec2.SecurityGroup.Id");
}

#[test]
fn nested_module_call_infers_unannotated_output_type_at_its_call_boundary() {
    assert_nested_module_call_mismatch("");
}

#[test]
fn depth_three_module_call_infers_unannotated_output_type_at_its_call_boundary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let web_tier = temp.path().join("web_tier");
    let needs_vpc = temp.path().join("needs_vpc");
    let middle = temp.path().join("middle");
    let outer = temp.path().join("outer");
    let root = temp.path().join("root");
    for directory in [&web_tier, &needs_vpc, &middle, &outer, &root] {
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
  sg_id = web_sg.group_id
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
        middle.join("main.crn"),
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
    .expect("middle module");
    std::fs::write(
        outer.join("main.crn"),
        r#"arguments {
  vpc_id: aws.ec2.Vpc.Id
}

let middle = use { source = '../middle' }
let m = middle {
  vpc_id = vpc_id
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
            diagnostic.contains("module call 'instance.m.b': argument 'vpc_id'")
                && diagnostic.contains("expected aws.ec2.Vpc.Id")
                && diagnostic.contains("got aws.ec2.SecurityGroup.Id")
                && diagnostic.contains("from instance.m.a.sg_id")
        })
        .collect();

    assert_eq!(
        mismatches.len(),
        1,
        "depth-three call boundaries must be checked exactly once: {diagnostics:#?}",
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
