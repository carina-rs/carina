//! End-to-end CLI parity tests for the type-check pipeline (#2247).
//!
//! Each scenario builds the same directory-shaped fixture as
//! `carina-lsp/tests/e2e_typecheck.rs` (#2215), runs both:
//!
//! 1. `DiagnosticEngine::analyze` (LSP path)
//! 2. `validate_with_factories` (CLI path, the production pipeline
//!    behind `carina validate` minus stdout formatting)
//!
//! and asserts the two diagnostic sets agree on count and message
//! substrings. Driving the CLI logic directly — rather than spawning
//! the `carina` binary as a subprocess — lets the test inject the
//! same hand-built schemas the LSP test uses without a WASM provider
//! plugin (the constraint that left `carina-cli/tests/negative_validation.rs`
//! `#[ignore]`-gated).
//!
//! Tests run unconditionally in CI. End-to-end coverage of the binary
//! itself also covers provider-availability regressions below, including
//! terminal warnings, error severity, and exit codes on the same input as LSP.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use carina_core::provider::{
    BoxFuture, NoopNormalizer, Provider, ProviderFactory, ProviderNormalizer, ProviderResult,
};
use carina_core::resource::{ConcreteValue, State, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField,
    ValidatedResourceSchema, legacy_validator,
};
use carina_lsp::diagnostics::DiagnosticEngine;
use carina_lsp::document::Document;
use tempfile::TempDir;
use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

#[path = "e2e_typecheck_parity/conditional_input_rules.rs"]
mod conditional_input_rules;

#[path = "e2e_typecheck_parity/round4.rs"]
mod round4;

#[path = "e2e_typecheck_parity/error_composition.rs"]
mod error_composition;

#[path = "e2e_typecheck_parity/seam_round1.rs"]
mod seam_round1;

#[path = "e2e_typecheck_parity/seam_round3.rs"]
mod seam_round3;

// ============================================================================
// Fixture + helpers (mirror carina-lsp/tests/e2e_typecheck.rs)
// ============================================================================

fn write_fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    for (name, body) in files {
        std::fs::write(dir.path().join(name), body).expect("write fixture file");
    }
    dir
}

// Mirrors carina-aws-types' IAM document: optional struct fields, principal
// Struct | String, String | List<String> leaves, and nested condition maps.
fn iam_document_schemas() -> SchemaRegistry {
    let strings = || {
        AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::unordered_list(AttributeType::string()),
        ])
    };
    let principal = AttributeType::union(vec![
        AttributeType::struct_(
            "Principal",
            ["service", "aws", "federated", "canonical_user"]
                .into_iter()
                .map(|name| StructField::optional(name, strings()).build())
                .collect(),
        ),
        AttributeType::string(),
    ]);
    let statement = AttributeType::struct_(
        "Statement",
        vec![
            StructField::optional("sid", AttributeType::string()).build(),
            StructField::optional("effect", AttributeType::string()).build(),
            StructField::optional("action", strings()).build(),
            StructField::optional("resource", strings()).build(),
            StructField::optional("principal", principal).build(),
            StructField::optional(
                "condition",
                AttributeType::map(AttributeType::map(strings())),
            )
            .build(),
        ],
    );
    let document = AttributeType::struct_(
        "PolicyDocument",
        vec![
            StructField::optional("version", AttributeType::string()).build(),
            StructField::optional("id", AttributeType::string()).build(),
            StructField::optional("statement", AttributeType::list(statement))
                .with_block_name("statement")
                .build(),
        ],
    );
    let named = |name| {
        AttributeType::refined_string(
            Some(carina_core::schema::TypeIdentity::from_schema_type(
                "test", "iam.Role", name,
            )),
            None,
            None,
            None,
        )
    };
    let mut schemas = single_schema_map(
        ResourceSchema::new("iam.Role")
            .attribute(AttributeSchema::optional("role_name", named("Name")).build())
            .attribute(AttributeSchema::provider_populated("arn", named("Arn")).build())
            .attribute(
                AttributeSchema::provider_populated("arns", AttributeType::list(named("Arn")))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("assume_role_policy_document", document.clone()).build(),
            ),
    );
    schemas.insert(
        "test",
        ResourceSchema::new("s3.BucketPolicy")
            .attribute(AttributeSchema::required("policy", document).build())
            .validate()
            .unwrap(),
    );
    schemas
}

fn assert_iam_document_valid(source: &str) {
    for sibling in ["a-source.crn", "z-source.crn"] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            (sibling, "let source = test.iam.Role {}\n"),
            ("main.crn", source),
        ]);
        let schemas = iam_document_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        assert!(cli.is_empty(), "{sibling}: {cli:#?}");
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert!(lsp.is_empty(), "{sibling}: {lsp:#?}");
    }
}

#[test]
fn iam_bucket_policy_multifile_principal_struct_union() {
    assert_iam_document_valid(
        r#"test.s3.BucketPolicy {
  policy = {
    version = '2012-10-17'
    statement {
      effect = 'Allow'
      principal = { aws = ["${source.arn}", 'arn:aws:iam::123456789012:root'] }
      action = 's3:GetObject'
      resource = "${source.arn}/*"
    }
  }
}
"#,
    );
}

#[test]
fn iam_role_policy_multifile_typed_principal_list_union() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider test {}\n"),
        ("source.crn", "let source = test.iam.Role {}\n"),
        (
            "main.crn",
            "let deploy = use { source = './deploy' }\nlet role = deploy { sso_admin_role_arns = source.arns }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("deploy")).unwrap();
    std::fs::write(
        fixture.path().join("deploy/arguments.crn"),
        "arguments { sso_admin_role_arns: list(test.iam.Role.Arn) }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("deploy/main.crn"),
        r#"let role = test.iam.Role {
  assume_role_policy_document = {
    statement {
      effect = 'Allow'
      principal = { aws = sso_admin_role_arns }
      action = 'sts:AssumeRole'
    }
  }
}
"#,
    )
    .unwrap();
    let schemas = iam_document_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(cli.is_empty(), "{cli:#?}");
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "deploy/main.crn");
    assert!(
        lsp.iter().all(|diagnostic| diagnostic.severity
            != Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)),
        "{lsp:#?}"
    );
}

#[test]
fn iam_trust_document_multifile_struct_and_condition_map_unions() {
    assert_iam_document_valid(
        r#"let role = test.iam.Role {
  assume_role_policy_document = {
    statement {
      effect = 'Allow'
      principal = { aws = ["${source.arn}", "${source.arns[0]}"] }
      action = ['sts:AssumeRole', 'sts:TagSession']
      condition = { string_equals = { 'aws:PrincipalArn' = ["${source.arn}"] } }
    }
    statement {
      effect = 'Allow'
      principal = '*'
      action = 'sts:AssumeRole'
    }
  }
}
"#,
    );
}

#[test]
fn iam_module_string_argument_keeps_main_diagnostic_text_and_owner() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider test {}\n"),
        (
            "main.crn",
            "let role = use { source = './child' }\nlet instance = role { role_name = 'example' }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(
        fixture.path().join("child/arguments.crn"),
        "arguments { role_name: String }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("child/main.crn"),
        "let role = test.iam.Role {\n  role_name = role_name\n}\n",
    )
    .unwrap();
    let cli: Vec<_> = cli_diagnostics(factories_for(&iam_document_schemas()), &fixture)
        .into_iter()
        .map(|message| message.replace(&format!("{}/", fixture.path().display()), ""))
        .collect();
    let mismatch = "Type mismatch: expected test.iam.Role.Name, got String (from role_name)";
    assert_eq!(
        cli,
        vec![
            "child: test.iam.Role.role: cannot assign String to 'role_name': expected test.iam.Role.Name, got String (from role_name)".to_string(),
            format!("child/main.crn at line 2\n{mismatch}"),
            format!("child/main.crn at line 2\nchild/main.crn: {mismatch} at line 2\nchild/main.crn at line 2\n{mismatch}"),
        ],
        "module binding checks must own the same diagnostics as main"
    );
}

#[test]
fn scheduler_join_multifile_accepts_mixed_scalars_and_keeps_operand_errors() {
    for (item, accepted) in [("interval", true), ("('present' ?? interval)", false)] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("interval.crn", "let interval = 5\n"),
            (
                "main.crn",
                &format!("test.r.typed {{ name = join('', ['rate(', {item}, ' minutes)']) }}\n"),
            ),
        ]);
        let schemas = expression_typing_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        assert_eq!(cli.is_empty(), accepted, "{item}: {cli:#?}");
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert_eq!(
            lsp_error_multiset(&lsp).is_empty(),
            accepted,
            "{item}: {lsp:#?}"
        );
    }
}

#[test]
fn cloudfront_multifile_shared_struct_definitions_keep_field_types() {
    let behavior = AttributeType::struct_(
        "CacheBehavior",
        vec![
            StructField::optional("forwarded_values", AttributeType::ref_("ForwardedValues"))
                .build(),
        ],
    );
    let mut schemas = expression_typing_schemas();
    schemas.insert(
        "test",
        ResourceSchema::new("cloudfront.Distribution")
            .attribute(
                AttributeSchema::required(
                    "distribution_config",
                    AttributeType::struct_(
                        "DistributionConfig",
                        vec![
                            StructField::optional("comment", AttributeType::string()).build(),
                            StructField::optional("default_cache_behavior", behavior.clone())
                                .build(),
                            StructField::optional("cache_behaviors", AttributeType::list(behavior))
                                .build(),
                        ],
                    ),
                )
                .build(),
            )
            .with_def(
                "ForwardedValues",
                AttributeType::struct_(
                    "ForwardedValues",
                    vec![
                        StructField::required("query_string", AttributeType::bool()).build(),
                        StructField::optional("cookies", AttributeType::ref_("Cookies")).build(),
                    ],
                ),
            )
            .with_def(
                "Cookies",
                AttributeType::struct_(
                    "Cookies",
                    vec![StructField::optional("forward", AttributeType::string()).build()],
                ),
            )
            .validate()
            .unwrap(),
    );
    for (query_string, accepted) in [("false", true), ("'wrong'", false)] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("source.crn", "let source = test.r.typed {}\n"),
            (
                "main.crn",
                &format!(
                    r#"test.cloudfront.Distribution {{
  distribution_config = {{
    comment = "${{source.value}} distribution"
    default_cache_behavior = {{
      forwarded_values = {{ query_string = {query_string} cookies = {{ forward = 'none' }} }}
    }}
    cache_behaviors = [{{
      forwarded_values = {{ query_string = false cookies = {{ forward = 'none' }} }}
    }}]
  }}
}}
"#
                ),
            ),
        ]);
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        assert_eq!(cli.is_empty(), accepted, "{query_string}: {cli:#?}");
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
        assert_eq!(lsp.is_empty(), accepted, "{query_string}: {lsp:#?}");
    }
}

fn expression_typing_schemas() -> SchemaRegistry {
    let mut schemas = single_schema_map(
        ResourceSchema::new("r.typed")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("value", AttributeType::string()).build(),
            )
            .attribute(
                AttributeSchema::provider_populated(
                    "names",
                    AttributeType::list(AttributeType::string()),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    "role_name",
                    AttributeType::refined_string(
                        Some(carina_core::schema::TypeIdentity::new(
                            Some("test"),
                            ["r"],
                            "RoleName",
                        )),
                        Some(carina_core::schema::PatternSpec::new(
                            "^[a-z]+$",
                            carina_core::schema::PatternMatch::Full,
                        )),
                        None,
                        None,
                    ),
                )
                .build(),
            ),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("s3.Bucket")
            .attribute(AttributeSchema::optional("bucket_name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas
}

#[test]
fn value_dependent_bindings_keep_cli_lsp_directory_parity_at_every_sink() {
    for (definition, accesses, ty) in [
        (
            "let cfg = lookup({ prod = { name = 'a' } }, 'prod', {})",
            &[
                "cfg.name",
                "cfg['name']",
                "(cfg).name",
                "alias.name",
                "cfg[key]",
            ][..],
            "String",
        ),
        (
            "let cfg = map('.inner', { prod = { inner = { name = 'a' } } })",
            &["cfg.prod.name", "cfg['prod']['name']", "cfg.prod[key]"][..],
            "String",
        ),
        (
            "let cfg = { name = 'a' enabled = true }",
            &["cfg.name", "cfg['name']", "cfg[key]"][..],
            "String",
        ),
        (
            "fn make() { { name = 'a' } }\nlet cfg = make()",
            &["cfg.name", "cfg['name']", "cfg[key]"][..],
            "String",
        ),
        (
            "let cfg = { name = min(1, 2) }",
            &["cfg.name", "cfg['name']", "cfg[key]"][..],
            "Int",
        ),
        (
            "let cfg = { name = max(1, 2) }",
            &["cfg.name", "cfg['name']", "cfg[key]"][..],
            "Int",
        ),
    ] {
        for access in accesses {
            for sink in [
                format!("let m = template {{ name = {access} }}"),
                format!("attributes {{ result: {ty} = {access} }}"),
                format!("exports {{ result: {ty} = {access} }}"),
                format!("fn take(x: {ty}) {{ x }}\nlet y = take({access})"),
            ] {
                for origin in ["a-values.crn", "z-values.crn"] {
                    let main = format!(
                        "let key = 'name'\nlet template = use {{ source = './child' }}\n{sink}\n"
                    );
                    let fixture = write_fixture(&[
                        ("main.crn", &main),
                        (origin, &format!("{definition}\nlet alias = cfg\n")),
                    ]);
                    std::fs::create_dir(fixture.path().join("child")).unwrap();
                    std::fs::write(
                        fixture.path().join("child/main.crn"),
                        format!("arguments {{ name: {ty} }}\nattributes {{ result = name }}\n"),
                    )
                    .unwrap();
                    let schemas = SchemaRegistry::new();
                    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
                    assert!(cli.is_empty(), "{definition}\n{sink}: {cli:#?}");
                    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
                    assert!(lsp.is_empty(), "{definition}\n{sink}: {lsp:#?}");
                }
            }
        }
    }
}

#[test]
fn value_dependent_module_outputs_keep_cli_lsp_access_parity() {
    for access in ["produced.cfg.name", "produced.cfg['name']"] {
        let main = format!(
            "let producer = use {{ source = './producer' }}\nlet produced = producer {{}}\nlet consumer = use {{ source = './consumer' }}\nlet c = consumer {{ name = {access} }}\nexports {{ name: String = {access} }}\n"
        );
        let fixture = write_fixture(&[("main.crn", &main)]);
        for (module, source) in [
            (
                "producer",
                "attributes { cfg = lookup({ prod = { name = 'a' } }, 'prod', {}) }\n",
            ),
            ("consumer", "arguments { name: String }\n"),
        ] {
            std::fs::create_dir(fixture.path().join(module)).unwrap();
            std::fs::write(fixture.path().join(module).join("main.crn"), source).unwrap();
        }
        let schemas = SchemaRegistry::new();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        assert!(cli.is_empty(), "{access}: {cli:#?}");
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert!(lsp.is_empty(), "{access}: {lsp:#?}");
    }
}

#[test]
fn authored_expression_coalesce_cli_lsp_directory_parity() {
    for reverse in [false, true] {
        for expression in [
            "a ?? 42",
            "length([a ?? 42])",
            "['ok', (a ?? 42)][0]",
            "\"${a ?? 42}\"",
            "secret(a ?? 42)",
        ] {
            let main = format!("let b = {expression}\n");
            let origin = if reverse {
                "z-origin.crn"
            } else {
                "a-origin.crn"
            };
            let fixture = write_fixture(&[
                ("main.crn", &main),
                (origin, "let first = 'present'\n"),
                ("aliases.crn", "let a = first\n"),
                ("exports.crn", "exports {}\n"),
                ("providers.crn", "provider test {}\n"),
                ("backend.crn", "backend local { path = 'state.json' }\n"),
            ]);
            let schemas = expression_typing_schemas();
            let cli = cli_diagnostics(factories_for(&schemas), &fixture);
            assert_eq!(cli.len(), 1, "{expression}: {cli:#?}");
            assert!(
                cli[0].contains("String") && cli[0].contains("Int"),
                "{cli:#?}"
            );
            if expression.starts_with("secret(") {
                assert!(
                    cli[0].lines().last()
                        == Some("coalesce default type `Int` does not match left type `String`"),
                    "{cli:#?}"
                );
            }
            let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
            assert_eq!(
                lsp_error_multiset(&lsp),
                cli_located_error_multiset(&cli),
                "{expression}: {lsp:#?}"
            );
        }
    }
}

fn named_module_sink(through_let: bool) {
    for (interpolation, accepted) in [
        ("\"${source.value}\"", false),
        ("\"${source.value}-suffix\"", true),
    ] {
        let rhs = if through_let {
            "constructed"
        } else {
            interpolation
        };
        let alias = format!("let constructed = {interpolation}\n");
        let main = format!(
            "let source = test.r.typed {{}}\n{alias}let template = use {{ source = './child' }}\nlet child = template {{ role = {rhs} }}\n"
        );
        let fixture = write_fixture(&[
            ("main.crn", &main),
            ("exports.crn", "exports {}\n"),
            ("providers.crn", "provider test {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
        ]);
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/main.crn"),
            "arguments { role: test.r.RoleName }\nattributes { result = role }\n",
        )
        .unwrap();
        let schemas = expression_typing_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        assert_eq!(cli.is_empty(), accepted, "{main}: {cli:#?}");
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert_eq!(
            lsp_error_multiset(&lsp).is_empty(),
            accepted,
            "{main}: {lsp:#?}"
        );
    }
}

#[test]
fn inline_named_sink_preserves_interpolation_policy() {
    named_module_sink(false);
}

#[test]
fn let_named_sink_preserves_interpolation_policy() {
    named_module_sink(true);
}

fn loop_refined_sink_parity(expression: &str, accepted: bool) {
    let fixture = write_fixture(&[
        ("main.crn", expression),
        (
            "source.crn",
            "let source = test.r.typed {}\nlet roles = { admin = true }\n",
        ),
        ("providers.crn", "provider test {}\n"),
        ("exports.crn", "exports {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ]);
    let schemas = expression_typing_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert_eq!(cli.is_empty(), accepted, "{expression}: {cli:#?}");
    assert_eq!(lsp.is_empty(), accepted, "{expression}: {lsp:#?}");
    if !accepted {
        assert_eq!(cli.len(), 1, "{cli:#?}");
        assert_eq!(lsp.len(), 1, "{lsp:#?}");
        assert!(cli[0].contains("from source.value"), "{cli:#?}");
        assert!(lsp[0].message.contains("from source.value"), "{lsp:#?}");
    }
}

#[test]
fn bare_loop_map_key_enters_refined_sink_in_cli_and_lsp() {
    loop_refined_sink_parity(
        "for k, _ in roles { test.r.typed { role_name = k } }\n",
        true,
    );
}

#[test]
fn bare_loop_schema_list_element_enters_refined_sink_in_cli_and_lsp() {
    loop_refined_sink_parity(
        "for n in source.names { test.r.typed { role_name = n } }\n",
        true,
    );
}

#[test]
fn loop_reference_list_keeps_source_assignment_checks_in_cli_and_lsp() {
    loop_refined_sink_parity(
        "for n in [source.value] { test.r.typed { role_name = n } }\n",
        false,
    );
}

#[test]
fn attribute_parameter_mismatch_has_exactly_one_lsp_diagnostic() {
    let source = "attributes { result: Int = \"${source.value}\" }\n";
    let fixture = write_fixture(&[
        ("main.crn", source),
        ("source.crn", "let source = test.r.typed {}\n"),
        ("providers.crn", "provider test {}\n"),
    ]);
    let diagnostics = lsp_diagnostics(
        &engine_with_schemas(expression_typing_schemas()),
        &fixture,
        "main.crn",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(diagnostics[0].message.contains("Int"), "{diagnostics:#?}");
}

#[test]
fn declared_string_sink_policy_cli_lsp_parity() {
    for (sink, accepted) in [("'admin' | 'guest'", true), ("test.r.RoleName", false)] {
        let source =
            format!("arguments {{ name: String }}\nattributes {{ result: {sink} = name }}\n");
        let fixture = write_fixture(&[
            (
                "main.crn",
                "let template = use { source = './child' }\nlet child = template { name = 'admin' }\n",
            ),
            ("providers.crn", "provider test {}\n"),
            ("exports.crn", "exports {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
        ]);
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(fixture.path().join("child/main.crn"), &source).unwrap();
        let schemas = expression_typing_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "child/main.crn");
        assert_eq!(cli.is_empty(), accepted, "{sink}: {cli:#?}");
        assert_eq!(lsp.is_empty(), accepted, "{sink}: {lsp:#?}");
    }
}

#[test]
fn struct_literal_leaf_type_mismatch_cli_lsp_parity() {
    let source = "attributes { result: { leaf: Int } = { leaf = 'bad' } }\n";
    let fixture = write_fixture(&[("main.crn", source)]);
    let schemas = SchemaRegistry::new();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert_eq!(cli.len(), 1, "{cli:#?}");
    assert_eq!(lsp.len(), 1, "{lsp:#?}");
    assert!(
        cli[0].contains("leaf") && cli[0].contains("Int"),
        "{cli:#?}"
    );
    assert!(
        lsp[0].message.contains("leaf") && lsp[0].message.contains("Int"),
        "{lsp:#?}"
    );
}

#[test]
fn structural_string_kinds_have_cli_lsp_directory_parity() {
    for (expression, accepted) in [
        ("\"${source.value}\"", false),
        ("constructed", false),
        ("source.value", false),
        ("[\"${source.value}\"][0]", false),
        ("\"${source.value}-inline\"", true),
        ("\"a${source.value}\"", true),
        ("\"${source.value}${source.value}\"", true),
        ("\"${source.value}\" ?? source.value", false),
        ("source.value ?? \"${source.value}\"", false),
        (
            "if true { \"${source.value}\" } else { source.value }",
            false,
        ),
        (
            "if false { source.value } else { \"${source.value}\" }",
            false,
        ),
        (
            "if true { \"${source.value}\" } else { \"${source.value}\" }",
            false,
        ),
    ] {
        let main = format!("test.r.typed {{ role_name = {expression} }}\n");
        let fixture = write_fixture(&[
            ("main.crn", &main),
            ("source.crn", "let source = test.r.typed {}\n"),
            ("alias.crn", "let constructed = \"${source.value}\"\n"),
            ("providers.crn", "provider test {}\n"),
            ("exports.crn", "exports {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
        ]);
        let schemas = expression_typing_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert_eq!(cli.is_empty(), accepted, "{expression}: {cli:#?}");
        let type_diagnostics: Vec<_> = lsp
            .iter()
            .filter(|diagnostic| {
                diagnostic.message.contains("RoleName") && diagnostic.message.contains("String")
            })
            .collect();
        assert_eq!(
            type_diagnostics.is_empty(),
            accepted,
            "{expression}: {lsp:#?}"
        );
    }
}

#[test]
fn if_and_for_keep_the_non_string_floor_in_cli_and_lsp() {
    for (sink, expression) in [
        (
            "Int",
            "if true { \"${source.value}\" } else { \"${source.value}\" }",
        ),
        (
            "Bool",
            "if false { \"${source.value}\" } else { \"${source.value}\" }",
        ),
        (
            "list(Int)",
            "[if true { \"${source.value}\" } else { \"${source.value}\" }]",
        ),
        (
            "list(Int)",
            "for n in [1, 2] { test.r.typed { name = \"${n}\" } }",
        ),
    ] {
        let main = format!("exports {{ result: {sink} = {expression} }}\n");
        let fixture = write_fixture(&[
            ("main.crn", &main),
            ("source.crn", "let source = test.r.typed {}\n"),
            ("providers.crn", "provider test {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
        ]);
        let schemas = expression_typing_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert!(!cli.is_empty(), "{main}");
        assert!(!lsp_error_multiset(&lsp).is_empty(), "{main}: {lsp:#?}");
    }
}

#[test]
fn declared_module_string_kind_is_independent_of_callers() {
    let fixture = write_fixture(&[
        (
            "main.crn",
            "let template = use { source = './child' }\nlet first = template { name = 'valid' }\nlet second = template { name = \"${source.value}\" }\n",
        ),
        ("source.crn", "let source = test.r.typed {}\n"),
        ("providers.crn", "provider test {}\n"),
        ("exports.crn", "exports {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ]);
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(
        fixture.path().join("child/main.crn"),
        "arguments { name: String }\nattributes { result: test.r.RoleName = name }\n",
    )
    .unwrap();
    let schemas = expression_typing_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let standalone_cli = cli_diagnostics_at(factories_for(&schemas), &fixture.path().join("child"));
    let engine = engine_with_schemas(schemas);
    let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
    let standalone = lsp_diagnostics(&engine, &fixture, "child/main.crn");
    let mismatch = |message: &str| message.contains("RoleName") && message.contains("String");
    assert!(cli.iter().any(|message| mismatch(message)), "{cli:#?}");
    assert!(
        standalone_cli.iter().any(|message| mismatch(message)),
        "{standalone_cli:#?}"
    );
    assert!(
        lsp.iter().any(|diagnostic| mismatch(&diagnostic.message)),
        "{lsp:#?}"
    );
    assert!(
        standalone
            .iter()
            .any(|diagnostic| mismatch(&diagnostic.message)),
        "{standalone:#?}"
    );
}

#[test]
fn authored_expression_inline_awscc_fallback_is_rejected() {
    let fixture = write_fixture(&[
        ("main.crn", "awscc.s3.Bucket { bucket_name = a ?? 42 }\n"),
        ("origin.crn", "let a = 'present'\n"),
        ("exports.crn", "exports {}\n"),
        ("providers.crn", "provider awscc {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ]);
    let schemas = expression_typing_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    assert_eq!(cli.len(), 1, "{cli:#?}");
    assert!(
        cli[0].contains("String") && cli[0].contains("Int"),
        "{cli:#?}"
    );
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert_eq!(
        lsp_error_multiset(&lsp),
        cli_located_error_multiset(&cli),
        "{lsp:#?}"
    );
}

#[test]
fn single_reference_interpolation_scalar_string_constraints_cli_lsp() {
    let schemas = single_schema_map(
        ResourceSchema::new("r.scalar")
            .attribute(AttributeSchema::provider_populated("port", AttributeType::int()).build())
            .attribute(AttributeSchema::optional("plain", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    "digits",
                    AttributeType::refined_string(
                        Some(carina_core::schema::TypeIdentity::new(
                            Some("test"),
                            ["r"],
                            "Digits",
                        )),
                        Some(carina_core::schema::PatternSpec::new(
                            "^[0-9]+$",
                            carina_core::schema::PatternMatch::Full,
                        )),
                        Some((Some(2), Some(4))),
                        None,
                    ),
                )
                .build(),
            ),
    );
    let engine = engine_with_schemas(schemas.clone());
    for module in [false, true] {
        for (origin, attribute, accepted) in [
            ("let port = 42", "plain", true),
            ("let port = source.port", "plain", true),
            ("let port = 42", "digits", true),
            ("let port = 1", "digits", false),
            ("let port = 12345", "digits", false),
            ("let port = -42", "digits", false),
            ("let port = source.port", "digits", true),
        ] {
            let fixture = write_fixture(&[
                ("providers.crn", "provider test {}\n"),
                ("source.crn", "let source = test.r.scalar {}\n"),
                ("aliases.crn", &format!("{origin}\n")),
                ("main.crn", ""),
            ]);
            if module {
                std::fs::create_dir(fixture.path().join("child")).unwrap();
                let ty = if attribute == "plain" {
                    "String"
                } else {
                    "test.r.Digits"
                };
                std::fs::write(
                    fixture.path().join("child/arguments.crn"),
                    format!("arguments {{ value: {ty} }}\n"),
                )
                .unwrap();
                std::fs::write(
                    fixture.path().join("child/main.crn"),
                    "attributes { result = value }\n",
                )
                .unwrap();
            }
            let source = if module {
                "let template = use { source = './child' }\nlet sink = template { value = \"${port}\" }\n".to_string()
            } else {
                format!("let sink = test.r.scalar {{ {attribute} = \"${{port}}\" }}\n")
            };
            std::fs::write(fixture.path().join("main.crn"), &source).unwrap();
            let cli = cli_diagnostics(factories_for(&schemas), &fixture);
            assert_eq!(
                cli.is_empty(),
                accepted,
                "{module}: {origin}: {source}: {cli:#?}"
            );
            let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
            assert_eq!(
                lsp_error_multiset(&lsp).is_empty(),
                accepted,
                "{module}: {origin}: {source}: {lsp:#?}"
            );
        }
    }
}

#[test]
fn single_reference_interpolation_at_module_call_matches_direct_diagnostics() {
    for (attribute, accepted) in [("value", false), ("role_name", true)] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("source.crn", "let source = test.r.typed {}\n"),
            ("aliases.crn", &format!("let x = source.{attribute}\n")),
            ("main.crn", ""),
        ]);
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/arguments.crn"),
            "arguments { value: test.r.RoleName }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("child/main.crn"),
            "attributes { result = value }\n",
        )
        .unwrap();
        let source = |rhs| {
            format!(
                "let template = use {{ source = './child' }}\nlet sink = template {{ value = {rhs} }}\n"
            )
        };
        std::fs::write(fixture.path().join("main.crn"), source("x")).unwrap();
        let schemas = expression_typing_schemas();
        let engine = engine_with_schemas(schemas.clone());
        let direct_cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let direct_lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
        assert_eq!(direct_cli.is_empty(), accepted, "{direct_cli:#?}");
        for rhs in ["\"${x}\"", "\"${ x }\""] {
            std::fs::write(fixture.path().join("main.crn"), source(rhs)).unwrap();
            let cli = cli_diagnostics(factories_for(&schemas), &fixture);
            assert_eq!(cli, direct_cli, "{rhs}");
            let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
            assert_eq!(
                lsp_error_multiset(&lsp),
                lsp_error_multiset(&direct_lsp),
                "{lsp:#?}"
            );
        }
    }
}

#[test]
fn single_reference_interpolation_resource_alias_matches_direct_diagnostics() {
    for (origin, accepted) in [
        ("let x = source.value\n", false),
        ("let x = source.role_name\n", true),
        ("let x = 'valid'\n", true),
        ("let x = 'INVALID'\n", false),
    ] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("source.crn", "let source = test.r.typed {}\n"),
            ("aliases.crn", origin),
            (
                "main.crn",
                "let sink = test.r.typed {\n  role_name = x\n}\n",
            ),
        ]);
        let schemas = expression_typing_schemas();
        let direct_cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let engine = engine_with_schemas(schemas.clone());
        let direct_lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
        assert_eq!(direct_cli.is_empty(), accepted, "{origin}: {direct_cli:#?}");
        assert_eq!(direct_lsp.is_empty(), accepted, "{origin}: {direct_lsp:#?}");
        for rhs in ["\"${x}\"", "\"${ x }\""] {
            std::fs::write(
                fixture.path().join("main.crn"),
                format!("let sink = test.r.typed {{\n  role_name = {rhs}\n}}\n"),
            )
            .unwrap();
            let cli = cli_diagnostics(factories_for(&schemas), &fixture);
            assert_eq!(cli, direct_cli, "{origin}: {rhs}");
            let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
            assert_eq!(lsp.len(), direct_lsp.len(), "{origin}: {rhs}: {lsp:#?}");
            assert_eq!(lsp_error_multiset(&lsp), lsp_error_multiset(&direct_lsp));
            for (diagnostic, direct) in lsp.iter().zip(&direct_lsp) {
                assert_eq!(
                    diagnostic.range.start, direct.range.start,
                    "{diagnostic:#?}"
                );
            }
        }
    }
}

#[test]
fn single_reference_interpolation_module_argument_matches_direct_diagnostics() {
    for (ty, attribute, accepted) in [
        ("String", "value", false),
        ("test.r.RoleName", "role_name", true),
    ] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("source.crn", "let source = test.r.typed {}\n"),
            (
                "main.crn",
                &format!(
                    "let template = use {{ source = './child' }}\nlet child = template {{ name = source.{attribute} }}\n"
                ),
            ),
        ]);
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/arguments.crn"),
            format!("arguments {{ name: {ty} }}\n"),
        )
        .unwrap();
        let sink = fixture.path().join("child/main.crn");
        std::fs::write(&sink, "test.r.typed {\n  role_name = name\n}\n").unwrap();
        let schemas = expression_typing_schemas();
        let direct_cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let engine = engine_with_schemas(schemas.clone());
        let direct_lsp = lsp_diagnostics(&engine, &fixture, "child/main.crn");
        let siblings = ["main.crn", "source.crn", "child/arguments.crn"];
        let direct_siblings: Vec<_> = siblings
            .iter()
            .map(|sibling| lsp_diagnostics(&engine, &fixture, sibling))
            .collect();
        assert_eq!(direct_cli.is_empty(), accepted, "{ty}: {direct_cli:#?}");
        assert_eq!(
            lsp_error_multiset(&direct_lsp).values().sum::<usize>(),
            usize::from(!accepted),
            "{direct_lsp:#?}"
        );
        std::fs::write(&sink, "test.r.typed {\n  role_name = \"${name}\"\n}\n").unwrap();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        assert_eq!(cli, direct_cli, "{ty}");
        let lsp = lsp_diagnostics(&engine, &fixture, "child/main.crn");
        assert_eq!(lsp.len(), direct_lsp.len(), "{ty}: {lsp:#?}");
        assert_eq!(lsp_error_multiset(&lsp), lsp_error_multiset(&direct_lsp));
        for (sibling, direct) in siblings.iter().zip(direct_siblings) {
            let diagnostics = lsp_diagnostics(&engine, &fixture, sibling);
            assert_eq!(diagnostics, direct, "{ty}: {sibling}");
        }
    }
}

#[test]
fn authored_expression_interpolation_inline_let_agree_on_admission() {
    for (interpolation, accepted) in [
        ("\"${x}\"", false),
        ("\"${x}-inline\"", true),
        ("\"a${x}\"", true),
        ("\"${x}${x}\"", true),
    ] {
        for named in [false, true] {
            for through_let in [false, true] {
                let rhs = if through_let { "alias" } else { interpolation };
                let binding = if named { "let sink = " } else { "" };
                let main = format!("{binding}test.r.typed {{ role_name = {rhs} }}\n");
                let alias = format!("let constructed = {interpolation}\nlet alias = constructed\n");
                let fixture = write_fixture(&[
                    ("main.crn", &main),
                    ("aliases.crn", &alias),
                    (
                        "origin.crn",
                        "let source = test.r.typed {}\nlet x = source.value\n",
                    ),
                    ("exports.crn", "exports {}\n"),
                    ("providers.crn", "provider test {}\n"),
                    ("backend.crn", "backend local { path = 'state.json' }\n"),
                ]);
                let schemas = expression_typing_schemas();
                let cli = cli_diagnostics(factories_for(&schemas), &fixture);
                assert_eq!(cli.is_empty(), accepted, "{main}: {cli:#?}");
                let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
                assert_eq!(
                    lsp_error_multiset(&lsp).is_empty(),
                    accepted,
                    "{main}: {lsp:#?}"
                );
            }
        }
    }
}

#[test]
fn authored_expression_plain_reference_and_concrete_interpolation_keep_constraints() {
    for (origin, rhs) in [
        ("let source = test.r.typed {}\nlet x = source.value\n", "x"),
        ("let x = 'INVALID'\n", "\"${x}\""),
    ] {
        let main = format!("test.r.typed {{ role_name = {rhs} }}\n");
        let fixture = write_fixture(&[
            ("main.crn", &main),
            ("origin.crn", origin),
            ("exports.crn", "exports {}\n"),
            ("providers.crn", "provider test {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
        ]);
        let schemas = expression_typing_schemas();
        assert!(
            !cli_diagnostics(factories_for(&schemas), &fixture).is_empty(),
            "{main}"
        );
        assert!(
            !lsp_error_multiset(&lsp_diagnostics(
                &engine_with_schemas(schemas),
                &fixture,
                "main.crn"
            ))
            .is_empty(),
            "{main}"
        );
    }
}

#[test]
fn authored_expression_module_string_interpolation_inline_let_agree() {
    for (rhs, accepted) in [
        ("\"${x}\"", false),
        ("\"${x}-suffix\"", true),
        ("alias", false),
    ] {
        let fixture = write_fixture(&[
            (
                "main.crn",
                "let template = use { source = './child' }\nlet child = template { x = source.value }\n",
            ),
            ("origin.crn", "let source = test.r.typed {}\n"),
            ("exports.crn", "exports {}\n"),
            ("providers.crn", "provider test {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
        ]);
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/main.crn"),
            format!("test.r.typed {{ role_name = {rhs} }}\n"),
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("child/arguments.crn"),
            "arguments { x: String }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("child/aliases.crn"),
            "let text = \"${x}\"\nlet alias = text\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("child/attributes.crn"),
            "attributes {}\n",
        )
        .unwrap();
        let schemas = expression_typing_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        assert_eq!(cli.is_empty(), accepted, "{rhs}: {cli:#?}");
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "child/main.crn");
        assert_eq!(
            lsp_error_multiset(&lsp).is_empty(),
            accepted,
            "{rhs}: {lsp:#?}"
        );
    }
}

// #4282: a string built by a builtin must enter a named string field through
// the same admission and resolved-value checks as mixed interpolation.
fn assert_module_constructed_string(
    expression: &str,
    attribute: &str,
    expected_rule_error: Option<&str>,
) {
    let fixture = write_fixture(&[
        ("providers.crn", "provider test {}\n"),
        (
            "main.crn",
            "let template = use { source = './child' }\nlet child = template { n = 360 }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(
        fixture.path().join("child/arguments.crn"),
        "arguments { n: Int }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("child/main.crn"),
        format!("test.iam.Role {{ {attribute} = {expression} }}\n"),
    )
    .unwrap();
    let schemas = single_schema_map(
        ResourceSchema::new("iam.Role")
            .attribute(
                AttributeSchema::optional(
                    "role_name",
                    AttributeType::refined_string(
                        Some(carina_core::schema::TypeIdentity::from_schema_type(
                            "test", "iam.Role", "Name",
                        )),
                        Some(carina_core::schema::PatternSpec::new(
                            "^carina-[0-9]+-role$",
                            carina_core::schema::PatternMatch::Full,
                        )),
                        Some((Some(1), Some(16))),
                        None,
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::optional("plain", AttributeType::string()).build()),
    );
    // Collect both paths before asserting so a CLI failure also reports LSP.
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "child/main.crn");
    let lsp_errors = lsp_error_multiset(&lsp);
    eprintln!("{attribute} = {expression}\nCLI: {cli:#?}\nLSP: {lsp:#?}");
    match expected_rule_error {
        None => assert!(
            cli.is_empty() && lsp_errors.is_empty(),
            "{attribute} = {expression} must be accepted in CLI and LSP\nCLI: {cli:#?}\nLSP: {lsp:#?}"
        ),
        Some(expected) => assert!(
            !cli.is_empty()
                && !lsp_errors.is_empty()
                && cli.iter().all(|message| message.contains(expected))
                && lsp_errors
                    .keys()
                    .all(|diagnostic| diagnostic.message.contains(expected)),
            "{attribute} = {expression} must fail with {expected:?} in CLI and LSP\nCLI: {cli:#?}\nLSP: {lsp:#?}"
        ),
    }
}

#[test]
fn issue_4282_module_join_at_named_string_is_accepted() {
    assert_module_constructed_string("join('', ['carina-', n, '-role'])", "role_name", None);
}

#[test]
fn issue_4282_module_interpolation_at_named_string_is_accepted() {
    assert_module_constructed_string(r#""carina-${n}-role""#, "role_name", None);
}

#[test]
fn issue_4282_module_join_reports_named_string_pattern_rule() {
    assert_module_constructed_string(
        "join('', ['bad', 'x'])",
        "role_name",
        Some("does not match required pattern /^carina-[0-9]+-role$/"),
    );
}

#[test]
fn issue_4282_module_interpolation_reports_named_string_pattern_rule() {
    assert_module_constructed_string(
        r#""bad${'x'}""#,
        "role_name",
        Some("does not match required pattern /^carina-[0-9]+-role$/"),
    );
}

#[test]
fn issue_4282_module_join_reports_named_string_length_rule() {
    assert_module_constructed_string(
        "join('', ['carina-', '360000', '-role'])",
        "role_name",
        Some("length 18 is outside allowed range [1, 16]"),
    );
}

#[test]
fn issue_4282_module_interpolation_reports_named_string_length_rule() {
    assert_module_constructed_string(
        r#""carina-${'360000'}-role""#,
        "role_name",
        Some("length 18 is outside allowed range [1, 16]"),
    );
}

#[test]
fn issue_4282_module_join_at_plain_string_is_accepted() {
    assert_module_constructed_string("join('', ['carina-', n, '-role'])", "plain", None);
}

#[test]
fn issue_4282_module_trim_at_named_string_uses_interpolation_rules() {
    assert_module_constructed_string(r#"trim(" carina-${n}-role ")"#, "role_name", None);
    assert_module_constructed_string(
        "trim(' badx ')",
        "role_name",
        Some("does not match required pattern /^carina-[0-9]+-role$/"),
    );
    assert_module_constructed_string(
        "trim(' carina-360000-role ')",
        "role_name",
        Some("length 18 is outside allowed range [1, 16]"),
    );
}

#[test]
fn issue_4282_module_secret_join_admits_named_string_like_interpolation() {
    assert_module_constructed_string(
        "secret(join('', ['carina-', n, '-role']))",
        "role_name",
        None,
    );
    assert_module_constructed_string(r#"secret("carina-${n}-role")"#, "role_name", None);
}

#[test]
fn issue_4282_secret_join_rule_diagnostics_match_interpolation_and_stay_masked() {
    let marker = "PRIVATE-4282";
    let fixture = write_fixture(&[("providers.crn", "provider test {}\n"), ("main.crn", "")]);
    let schemas = expression_typing_schemas();
    let engine = engine_with_schemas(schemas.clone());
    let mut expected = None;
    for expression in [
        format!("secret(join('', ['{marker}', '-suffix']))"),
        format!(r#"secret("{marker}-${{'suffix'}}")"#),
    ] {
        std::fs::write(
            fixture.path().join("main.crn"),
            format!("test.r.typed {{ role_name = {expression} }}\n"),
        )
        .unwrap();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
        let errors = (cli_located_error_multiset(&cli), lsp_error_multiset(&lsp));
        assert!(!errors.0.is_empty() && !errors.1.is_empty());
        for error in errors.0.keys().chain(errors.1.keys()) {
            assert!(error.message.contains("secret"), "{error:?}");
            assert!(!error.message.contains(marker), "{error:?}");
        }
        assert!(!format!("{lsp:?}").contains(marker));
        match &expected {
            Some(expected) => assert_eq!(&errors, expected),
            None => expected = Some(errors),
        }
    }
}

#[test]
fn issue_4282_split_at_named_string_list_checks_values_in_cli_and_lsp() {
    let name = || {
        AttributeType::refined_string(
            Some(carina_core::schema::TypeIdentity::from_schema_type(
                "test", "Names", "Name",
            )),
            Some(carina_core::schema::PatternSpec::new(
                "^[a-z]+$",
                carina_core::schema::PatternMatch::Full,
            )),
            Some((Some(1), Some(3))),
            None,
        )
    };
    let schemas = single_schema_map(
        ResourceSchema::new("Names")
            .attribute(AttributeSchema::optional("names", AttributeType::list(name())).build()),
    );
    let fixture = write_fixture(&[("providers.crn", "provider test {}\n"), ("main.crn", "")]);
    let engine = engine_with_schemas(schemas.clone());
    for (input, expected_rule) in [
        ("abc,def", None),
        ("A,b", Some("does not match required pattern /^[a-z]+$/")),
        ("abc,abcd", Some("length 4 is outside allowed range [1, 3]")),
        (
            "abc,123",
            Some("does not match required pattern /^[a-z]+$/"),
        ),
    ] {
        for bound in [false, true] {
            let elements = input
                .split(',')
                .map(|value| format!(r#""${{''}}{value}""#))
                .collect::<Vec<_>>()
                .join(", ");
            let mut expected = None;
            let calls = input
                .split(',')
                .map(|value| format!("join('', ['{value}'])"))
                .collect::<Vec<_>>()
                .join(", ");
            for expression in [
                format!("split(',', '{input}')"),
                format!("[{elements}]"),
                format!("[{calls}]"),
            ] {
                let source = if bound {
                    format!("let names = {expression}\ntest.Names {{ names = names }}\n")
                } else {
                    format!("test.Names {{ names = {expression} }}\n")
                };
                std::fs::write(fixture.path().join("main.crn"), source).unwrap();
                let cli = cli_diagnostics(factories_for(&schemas), &fixture);
                let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
                let findings = lsp
                    .iter()
                    .filter(|diagnostic| {
                        matches!(
                            diagnostic.severity,
                            Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING),
                        )
                    })
                    .collect::<Vec<_>>();
                match expected_rule {
                    None => assert!(
                        cli.is_empty() && findings.is_empty(),
                        "{input}, bound={bound}: {cli:#?} {lsp:#?}"
                    ),
                    Some(rule) => {
                        assert!(
                            !cli.is_empty() && !findings.is_empty(),
                            "{input}, bound={bound}: CLI {cli:#?}; LSP {lsp:#?}"
                        );
                        assert!(cli.iter().all(|error| error.contains(rule)), "{cli:#?}");
                        assert!(
                            findings.iter().all(|error| error.message.contains(rule)),
                            "{lsp:#?}"
                        );
                    }
                }
                let signatures = findings
                    .iter()
                    .map(|diagnostic| format!("{:?}: {}", diagnostic.severity, diagnostic.message))
                    .collect::<Vec<_>>();
                let actual = (
                    cli_located_message_multiset(&cli),
                    message_multiset(signatures.iter().map(String::as_str)),
                );
                match &expected {
                    Some(expected) => assert_eq!(&actual, expected, "{input}, bound={bound}"),
                    None => expected = Some(actual),
                }
            }
        }
    }
}

fn engine_with_schemas(schemas: SchemaRegistry) -> DiagnosticEngine {
    let provider_names: Vec<String> = schemas
        .iter()
        .map(|(provider, _, _, _)| provider.to_string())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let factories = factories_for(&schemas);
    if provider_names.is_empty() {
        DiagnosticEngine::without_loaded_provider_schemas(carina_core::EnvironmentSnapshot::empty())
    } else {
        let loaded_providers = carina_core::provider::loaded_provider_snapshot_from_factories(
            Arc::new(factories),
            SchemaRegistry::new(),
            carina_core::EnvironmentSnapshot::empty(),
        );
        DiagnosticEngine::new(Arc::new(loaded_providers), provider_names)
    }
}

fn single_schema_map(schema: ResourceSchema) -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    schemas
}

fn fixture_enum_segment_as_aws_value(s: &str) -> String {
    // Mirrors the old last-dot-segment semantics for these dot-free fixture
    // values, then rewrites DSL underscores to AWS hyphens.
    s.split('.').next_back().unwrap_or(s).replace('_', "-")
}

/// Build a `ProviderFactory` set covering every provider name implied by
/// the schemas. The CLI pipeline keys schemas by provider, so each
/// distinct prefix (`test`, `aws`, ...) needs its own factory entry.
fn factories_for(schemas: &SchemaRegistry) -> Vec<Box<dyn ProviderFactory>> {
    let mut by_provider: HashMap<String, Vec<ValidatedResourceSchema>> = HashMap::new();
    for (provider, _resource_type, _kind, schema) in schemas.iter() {
        let provider = provider.to_string();
        by_provider
            .entry(provider)
            .or_default()
            .push(schema.clone());
    }
    by_provider
        .into_iter()
        .map(|(name, schemas)| {
            Box::new(TestProviderFactory { name, schemas }) as Box<dyn ProviderFactory>
        })
        .collect()
}

/// Run the LSP path on a fixture file and return its diagnostics.
fn lsp_diagnostics(
    engine: &DiagnosticEngine,
    fixture: &TempDir,
    file_name: &str,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let path = fixture.path().join(file_name);
    let text = std::fs::read_to_string(&path).expect("read fixture file");
    let doc = Document::new_with_path(
        text,
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    engine.analyze(&doc, Some(&path))
}

/// Run the CLI path on a fixture directory and return the diagnostic
/// strings the CLI would surface.
fn cli_diagnostics(factories: Vec<Box<dyn ProviderFactory>>, fixture: &TempDir) -> Vec<String> {
    cli_diagnostics_at(factories, fixture.path())
}

fn cli_diagnostics_at(
    factories: Vec<Box<dyn ProviderFactory>>,
    path: &std::path::Path,
) -> Vec<String> {
    carina_cli::commands::validate::validate_with_factories(
        &carina_core::paths::ResolvedPath::new(path).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories),
    )
}

fn cli_validated_file(
    factories: Vec<Box<dyn ProviderFactory>>,
    fixture: &TempDir,
) -> carina_core::parser::ExpandedInferredFile {
    let loaded = carina_core::config_loader::load_configuration_with_config(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        &SchemaRegistry::new(),
    )
    .expect("load CLI fixture");
    let parsed = loaded.parsed;
    let duplicate_declarations = loaded.duplicate_declarations;
    let providers = carina_cli::wiring::ProviderLoad::for_test_with_provider_phase(
        &carina_cli::test_support::TestCommandRuntime::new(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories),
        &parsed.providers,
    );
    let validated = providers
        .validate_evaluated_config(
            parsed,
            &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
            false,
            &duplicate_declarations,
        )
        .expect("CLI validation fixture must pass");
    validated.evaluated_resources_for_test().clone()
}

fn message_multiset<'a>(messages: impl IntoIterator<Item = &'a str>) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for message in messages {
        *counts.entry(message.to_string()).or_default() += 1;
    }
    counts
}

fn lsp_message_multiset(
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
) -> BTreeMap<String, usize> {
    message_multiset(
        diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.as_str()),
    )
}

fn cli_located_message_multiset(diagnostics: &[String]) -> BTreeMap<String, usize> {
    message_multiset(diagnostics.iter().map(|diagnostic| {
        if let Some((context, message)) = diagnostic.split_once('\n')
            && context
                .rsplit_once(" at line ")
                .is_some_and(|(_, line)| line.parse::<usize>().is_ok())
        {
            return message;
        }
        match diagnostic.rsplit_once(" at line ") {
            Some((message, line)) if line.parse::<usize>().is_ok() => message,
            Some((_message, _line)) => diagnostic,
            None => diagnostic,
        }
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CanonicalDiagnostic {
    message: String,
    severity: &'static str,
}

fn canonical_error_multiset(
    messages: impl IntoIterator<Item = String>,
) -> BTreeMap<CanonicalDiagnostic, usize> {
    let mut counts = BTreeMap::new();
    for message in messages {
        *counts
            .entry(CanonicalDiagnostic {
                message,
                severity: "ERROR",
            })
            .or_default() += 1;
    }
    counts
}

fn cli_located_error_multiset(diagnostics: &[String]) -> BTreeMap<CanonicalDiagnostic, usize> {
    canonical_error_multiset(
        cli_located_message_multiset(diagnostics)
            .into_iter()
            .flat_map(|(message, count)| std::iter::repeat_n(message, count)),
    )
}

fn lsp_error_multiset(
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
) -> BTreeMap<CanonicalDiagnostic, usize> {
    canonical_error_multiset(
        diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
            })
            .map(|diagnostic| diagnostic.message.clone()),
    )
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AnnotationDiagnostic {
    severity: &'static str,
    code: Option<String>,
    message: String,
}

fn cli_annotation_diagnostics(
    fixture: &TempDir,
    output: &std::process::Output,
    load_errors: &HashMap<String, String>,
) -> Vec<AnnotationDiagnostic> {
    use carina_cli::error::AppError;
    use carina_core::parser::WarningKind;

    // Read the typed production report before rendering. The subprocess below
    // independently verifies that validate prints those same severities,
    // counts and messages; terminal text does not expose diagnostic codes.
    let typed = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        Vec::new(),
        load_errors.clone(),
    );
    let mut diagnostics = Vec::new();
    for warning in typed.warnings {
        let code = match warning.kind {
            WarningKind::ProviderSchemasNotLoaded => "providers_not_loaded",
            other => panic!("unexpected CLI warning kind: {other:?}"),
        };
        diagnostics.push(AnnotationDiagnostic {
            severity: "WARNING",
            code: Some(code.to_string()),
            message: warning.message,
        });
    }
    for error in typed.errors {
        match &error {
            AppError::ModuleCall(finding)
                if finding.kind()
                    == carina_core::validation::ModuleCallFindingKind::TypeResolution => {}
            AppError::Validation(message)
                if load_errors.values().any(|reason| reason == message) =>
            {
                continue;
            }
            other => panic!("unexpected CLI error: {other:?}"),
        }
        let formatted = carina_cli::error::format_error_with_source_chain(&error);
        for (message, count) in cli_located_message_multiset(&[formatted]) {
            assert_eq!(count, 1);
            diagnostics.push(AnnotationDiagnostic {
                severity: "ERROR",
                code: None,
                message,
            });
        }
    }

    let stderr = std::str::from_utf8(&output.stderr).expect("CLI diagnostics must be UTF-8");
    let mut printed = Vec::new();
    let mut provider_errors = 0;
    let mut provider_notices = 0;
    for line in stderr.lines() {
        if load_errors
            .keys()
            .any(|provider| line.starts_with(&format!("Provider '{provider}' ")))
        {
            provider_notices += 1;
            continue;
        }
        if let Some(warning) = line.trim().strip_prefix('⚠') {
            let (_, message) = warning.split_once(": ").expect("located CLI warning");
            printed.push(("WARNING", message.to_string()));
            continue;
        }
        let message = line
            .strip_prefix("Error: ")
            .unwrap_or_else(|| panic!("unexpected stderr line: {line:?}; stderr: {stderr}"));
        // Each located finding's file/line header is rendered separately.
        if message
            .rsplit_once(" at line ")
            .is_some_and(|(file, line)| file.ends_with(".crn") && line.parse::<usize>().is_ok())
        {
            continue;
        }
        if load_errors
            .keys()
            .any(|provider| message.starts_with(&format!("Provider '{provider}' ")))
        {
            provider_errors += 1;
        } else {
            printed.push(("ERROR", message.to_string()));
        }
    }
    assert_eq!(provider_errors, load_errors.len(), "{stderr}");
    assert_eq!(provider_notices, load_errors.len(), "{stderr}");
    let mut expected = diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.severity, diagnostic.message.clone()))
        .collect::<Vec<_>>();
    expected.sort();
    printed.sort();
    assert_eq!(
        printed, expected,
        "subprocess and typed CLI report differ: {stderr}"
    );
    let success = load_errors.is_empty()
        && diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity != "ERROR");
    assert_eq!(output.status.success(), success, "{stderr}");
    if success {
        let json: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("successful validate JSON");
        assert_eq!(json["status"], "ok", "{json}");
        assert!(
            json.get("warnings").is_none(),
            "unexpected warnings: {json}"
        );
    } else {
        assert!(output.stdout.is_empty(), "failed validate emitted stdout");
    }
    diagnostics.sort();
    diagnostics
}

fn lsp_annotation_diagnostics(
    diagnostics: &[tower_lsp::lsp_types::Diagnostic],
    load_errors: &HashMap<String, String>,
) -> Vec<AnnotationDiagnostic> {
    use tower_lsp::lsp_types::{DiagnosticSeverity, NumberOrString};

    let mut annotations = Vec::new();
    let mut provider_errors = 0;
    for diagnostic in diagnostics {
        // Provider-loading notices have their own existing CLI/LSP rendering;
        // this comparison covers every provider-type diagnostic without filtering
        // errors or warnings by message text.
        if diagnostic.severity == Some(DiagnosticSeverity::INFORMATION) {
            assert!(
                load_errors.iter().any(|(provider, reason)| {
                    diagnostic.message == format!("Provider '{provider}' is not loaded: {reason}")
                }),
                "unexpected LSP notice: {diagnostic:#?}"
            );
            provider_errors += 1;
            continue;
        }
        let severity = match diagnostic.severity {
            Some(DiagnosticSeverity::WARNING) => "WARNING",
            Some(DiagnosticSeverity::ERROR) => "ERROR",
            other => panic!("unexpected LSP severity: {other:?}"),
        };
        let code = match &diagnostic.code {
            Some(NumberOrString::String(code)) => Some(code.clone()),
            None => None,
            other => panic!("unexpected LSP code: {other:?}"),
        };
        annotations.push(AnnotationDiagnostic {
            severity,
            code,
            message: diagnostic.message.clone(),
        });
    }
    assert_eq!(provider_errors, load_errors.len(), "{diagnostics:#?}");
    annotations.sort();
    annotations
}

fn assert_annotation_parity(
    source: &str,
    engine: DiagnosticEngine,
    failed_provider: bool,
    expected: &[(&str, Option<&str>, &str)],
) {
    let fixture = write_fixture(&[("main.crn", source)]);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
        .args(["validate", fixture.path().to_str().unwrap(), "--json"])
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .expect("run carina validate");
    let load_errors = if failed_provider {
        HashMap::from([(
            "broken".to_string(),
            "fixture provider unavailable".to_string(),
        )])
    } else {
        HashMap::new()
    };
    let cli = cli_annotation_diagnostics(&fixture, &output, &load_errors);
    let mut unmatched = cli.clone();
    for (severity, code, message) in expected {
        let index = unmatched
            .iter()
            .position(|diagnostic| {
                diagnostic.severity == *severity
                    && diagnostic.code.as_deref() == *code
                    && diagnostic.message.contains(message)
            })
            .unwrap_or_else(|| panic!("missing {severity} {code:?} {message:?}: {cli:#?}"));
        unmatched.remove(index);
    }
    assert!(
        unmatched.is_empty(),
        "unexpected CLI diagnostics: {unmatched:#?}"
    );

    let path = fixture.path().join("main.crn");
    let document = Document::new_with_path(
        source.to_string(),
        path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    // CLI validate receives a directory, so LSP must analyze that same
    // directory. An orphan buffer deliberately has different catalog authority.
    let diagnostics = engine
        .with_provider_errors(load_errors.clone())
        .analyze(&document, Some(&path));
    let lsp = lsp_annotation_diagnostics(&diagnostics, &load_errors);
    assert_eq!(
        lsp, cli,
        "CLI and LSP must agree on every type finding's severity, code, count, and message; source: {source}; diagnostics: {diagnostics:#?}"
    );
}

fn undeclared_type_engine() -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(TestProviderFactory {
        name: "mock".to_string(),
        schemas: Vec::new(),
    })];
    let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    DiagnosticEngine::new(Arc::new(loaded), vec!["mock".to_string()])
}

const FAILED_ANNOTATION_PROVIDER: &str =
    "provider broken { source = 'file:///definitely-missing/broken.wasm' }\n";

#[test]
fn providerless_directory_has_cli_lsp_strict_attribute_parity() {
    assert_annotation_parity(
        "attributes { x: AwsAccountId = '123' }\n",
        DiagnosticEngine::without_loaded_provider_schemas(carina_core::EnvironmentSnapshot::empty()),
        false,
        &[("ERROR", None, "unknown custom type 'AwsAccountId'")],
    );
}

#[test]
fn failed_provider_dotted_annotation_has_cli_lsp_deferral_parity() {
    assert_annotation_parity(
        &format!("{FAILED_ANNOTATION_PROVIDER}exports {{\n  x: broken.AccountId = 42\n}}\n"),
        DiagnosticEngine::without_loaded_provider_schemas(carina_core::EnvironmentSnapshot::empty()),
        true,
        &[(
            "WARNING",
            Some("providers_not_loaded"),
            "type check for broken.AccountId was deferred",
        )],
    );
}

#[test]
fn unloaded_bare_function_annotation_has_cli_lsp_deferral_parity() {
    for invocation in ["", "exports { x = echo('arn:aws:s3:::bucket') }\n"] {
        let mut expected = vec![(
            "WARNING",
            Some("providers_not_loaded"),
            "type check for Arn was deferred",
        )];
        if !invocation.is_empty() {
            expected.push(expected[0]);
        }
        assert_annotation_parity(
            &format!(
                "{FAILED_ANNOTATION_PROVIDER}fn echo(value: Arn): String {{ 'value' }}\n{invocation}"
            ),
            DiagnosticEngine::without_loaded_provider_schemas(
                carina_core::EnvironmentSnapshot::empty(),
            ),
            true,
            &expected,
        );
    }
}

#[test]
fn loaded_undeclared_bare_function_annotation_has_cli_lsp_error_parity() {
    for provider in ["", "provider mock {}\n"] {
        for invocation in ["", "exports { x = echo('arn:aws:s3:::bucket') }\n"] {
            assert_annotation_parity(
                &format!("{provider}fn echo(value: Arn): String {{ 'value' }}\n{invocation}"),
                undeclared_type_engine(),
                false,
                &[("ERROR", None, "unknown custom type 'Arn'")],
            );
        }
    }
}

#[test]
fn unrelated_declaration_error_keeps_cli_lsp_pending_call_parity() {
    for failed_call in ["", "exports { failed: String = bad('id') }\n"] {
        assert_annotation_parity(
            &format!(
                "provider mock {{}}\n{FAILED_ANNOTATION_PROVIDER}fn bad(x: mock.Missing) {{ x }}\nfn echo(x: Arn): String {{ 'ok' }}\n{failed_call}exports {{ x = echo('id') }}\n"
            ),
            undeclared_type_engine(),
            true,
            &[
                ("ERROR", None, "unknown custom type 'mock.Missing'"),
                (
                    "WARNING",
                    Some("providers_not_loaded"),
                    "type check for Arn was deferred",
                ),
                (
                    "WARNING",
                    Some("providers_not_loaded"),
                    "type check for Arn was deferred",
                ),
            ],
        );
    }
}

struct ModuleCallParityCase {
    name: &'static str,
    calls: &'static str,
    outer_arguments: &'static str,
    outer_calls: &'static str,
    expected: &'static [&'static str],
    expected_cli_line: Option<usize>,
    expected_lsp_range: Option<((u32, u32), (u32, u32))>,
}

fn module_call_parity_fixture(case: &ModuleCallParityCase) -> TempDir {
    let fixture = tempfile::tempdir().expect("module-call parity fixture");
    let outer = fixture.path().join("modules/outer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("imports.crn"),
        r#"provider test {}
let outer = use { source = "./modules/outer" }
let inner = use { source = "./modules/inner" }
"#,
    )
    .expect("write imports.crn");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        r#"let source = test.r.Source {
  name = "source"
  count = 7
}
"#,
    )
    .expect("write bindings.crn");
    std::fs::write(fixture.path().join("calls.crn"), case.calls).expect("write calls.crn");
    std::fs::write(outer.join("arguments.crn"), case.outer_arguments)
        .expect("write outer arguments");
    std::fs::write(
        outer.join("imports.crn"),
        "let inner = use { source = \"../inner\" }\n",
    )
    .expect("write outer imports");
    std::fs::write(outer.join("calls.crn"), case.outer_calls).expect("write outer calls");
    std::fs::write(
        inner.join("arguments.crn"),
        "arguments {\n  value: Int\n}\n",
    )
    .expect("write inner arguments");
    fixture
}

fn module_call_parity_schemas() -> SchemaRegistry {
    single_schema_map(
        ResourceSchema::new("r.Source")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(AttributeSchema::required("count", AttributeType::int()).build()),
    )
}

fn assert_module_resolution_expands(base_dir: &std::path::Path, schemas: &SchemaRegistry) {
    let loaded_providers = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories_for(schemas)),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let context = loaded_providers.provider_context();
    let schemas = loaded_providers.schema_registry();
    carina_core::validation::deferred_types::with_session_for_test(context, |session| {
        let directory = carina_core::config_loader::parse_directory_with_overrides_and_diagnostics(
            &carina_core::paths::ResolvedPath::new(base_dir).unwrap(),
            context,
            &HashMap::new(),
        )
        .expect("the fixture directory must parse before module expansion");
        let signatures = carina_core::module_resolver::load_resolved_module_signatures(
            &directory.parsed,
            &carina_core::paths::ResolvedPath::new(base_dir).unwrap(),
            context,
        );
        let configuration = carina_core::config_loader::load_buffer_configuration(
            &directory.parsed,
            schemas,
            context.as_provider_context(),
        );
        let binding_use_report = carina_core::validation::check_bare_binding_uses(
            &carina_core::validation::ValidationTypeContext::new(
                configuration.judged(),
                context,
                &signatures,
            ),
        );
        let bindings = carina_core::binding_index::BindingIndex::from_parsed_with_module_signatures(
            &directory.parsed,
            schemas,
            &signatures,
            session.catalog(),
        );
        let covered_findings = carina_core::validation::check_module_calls(
            carina_core::validation::ModuleCallCheckInput {
                authored: &directory.parsed,
                semantic: Some(&directory.parsed),
                base_dir: Some(&carina_core::paths::ResolvedPath::new(base_dir).unwrap()),
                imported_signatures: &signatures,
                provider_context: session,
                schemas,
                binding_use_report: &binding_use_report,
                bindings: &bindings,
                reference_checks: carina_core::validation::ModuleCallReferenceChecks::Enabled,
            },
        )
        .unwrap();
        match carina_core::module_resolver::resolve_modules_with_diagnostics(
            &directory.parsed,
            &carina_core::paths::ResolvedPath::new(base_dir).unwrap(),
            session,
            schemas,
            &covered_findings,
        ) {
            carina_core::module_resolver::ModuleResolutionDiagnosticResult::Expanded { graph } => {
                assert!(
                    !graph.compositions.is_empty(),
                    "the expansion proof must include a composition"
                );
            }
            carina_core::module_resolver::ModuleResolutionDiagnosticResult::ConstraintFailed {
                findings,
                errors,
            } => panic!(
                "fixture must reach expanded diagnostics; constraint findings={findings:#?}, residual count={}",
                errors.len(),
            ),
            carina_core::module_resolver::ModuleResolutionDiagnosticResult::Unowned { errors } => {
                panic!(
                    "fixture must reach expanded diagnostics; residual count={}",
                    errors.len()
                )
            }
        }
    });
}

#[test]
fn module_call_failures_have_exact_cli_lsp_error_parity_across_directory_files() {
    let cases = [
        ModuleCallParityCase {
            name: "unknown argument",
            calls: "outer {\n  value = \"ok\"\n  surprise = true\n}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "",
            expected: &["Unknown argument 'surprise' for module 'outer'"],
            expected_cli_line: None,
            expected_lsp_range: None,
        },
        ModuleCallParityCase {
            name: "missing required argument",
            calls: "outer {}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "",
            expected: &["Missing required argument 'value' for module 'outer'"],
            expected_cli_line: None,
            expected_lsp_range: None,
        },
        ModuleCallParityCase {
            name: "ordinary type mismatch",
            calls: "outer {\n  count = \"not-an-int\"\n}\n",
            outer_arguments: "arguments {\n  count: Int\n}\n",
            outer_calls: "",
            expected: &[
                "module call 'outer (anonymous call)': argument 'count': expected Int, got string 'not-an-int'",
            ],
            expected_cli_line: Some(2),
            expected_lsp_range: Some(((1, 10), (1, 22))),
        },
        ModuleCallParityCase {
            name: "attribute reference mismatch",
            calls: "outer {\n  value = source.count\n}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "",
            expected: &[
                "module call 'outer (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from source.count)",
            ],
            expected_cli_line: Some(2),
            expected_lsp_range: Some(((1, 10), (1, 22))),
        },
        ModuleCallParityCase {
            name: "nested outer to inner mismatch",
            calls: "outer {\n  value = \"root\"\n}\n",
            outer_arguments: "arguments {\n  value: String\n}\n",
            outer_calls: "inner {\n  value = value\n}\n",
            expected: &[
                "module call 'inner (anonymous call)': argument 'value': cannot assign String to 'value': expected Int, got String (from value)",
            ],
            expected_cli_line: None,
            expected_lsp_range: None,
        },
        ModuleCallParityCase {
            name: "for-expanded call",
            calls: r#"for item in ["first", "second"] {
  outer {
    count = "not-an-int"
  }
}
"#,
            outer_arguments: "arguments {\n  count: Int\n}\n",
            outer_calls: "",
            expected: &[
                "module call 'outer (anonymous call)': argument 'count': expected Int, got string 'not-an-int'",
            ],
            expected_cli_line: Some(3),
            expected_lsp_range: Some(((2, 12), (2, 24))),
        },
    ];

    for case in &cases {
        let schemas = module_call_parity_schemas();
        let fixture = module_call_parity_fixture(case);
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "calls.crn");
        let expected =
            canonical_error_multiset(case.expected.iter().map(|message| message.to_string()));

        assert_eq!(
            cli_located_error_multiset(&cli),
            expected,
            "{}: CLI must expose the exact canonical blocking findings; CLI={cli:#?}",
            case.name,
        );
        assert_eq!(
            lsp_error_multiset(&lsp),
            expected,
            "{}: LSP must expose the exact canonical ERROR findings; LSP={lsp:#?}",
            case.name,
        );
        assert_eq!(
            cli_located_error_multiset(&cli),
            lsp_error_multiset(&lsp),
            "{}: CLI and LSP must preserve the same message multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
            case.name,
        );
        if let Some(line) = case.expected_cli_line {
            assert!(
                cli.iter()
                    .any(|diagnostic| diagnostic.contains(&format!("calls.crn at line {line}\n"))),
                "{}: CLI must retain the authored argument-value line; CLI={cli:#?}",
                case.name,
            );
        }
        if let Some((start, end)) = case.expected_lsp_range {
            assert_eq!(
                lsp[0].range,
                tower_lsp::lsp_types::Range::new(
                    tower_lsp::lsp_types::Position::new(start.0, start.1),
                    tower_lsp::lsp_types::Position::new(end.0, end.1),
                ),
                "{}: LSP must underline the authored argument value",
                case.name,
            );
        }
    }
}

#[test]
fn nested_module_bare_binding_reference_mismatch_has_exact_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("nested binding-reference parity fixture");
    let outer = fixture.path().join("modules/outer");
    let producer = fixture.path().join("modules/producer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&producer).expect("create producer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("imports.crn"),
        "let outer = use { source = \"./modules/outer\" }\n",
    )
    .expect("write root import");
    std::fs::write(fixture.path().join("calls.crn"), "outer {}\n").expect("write root call");
    std::fs::write(
        outer.join("imports.crn"),
        r#"let producer = use { source = "../producer" }
let inner = use { source = "../inner" }
"#,
    )
    .expect("write nested imports");
    let nested_call = r#"let produced = producer { value = 7 }
let forwarded = produced.forwarded
inner { value = forwarded }
"#;
    std::fs::write(outer.join("calls.crn"), nested_call).expect("write nested call");
    std::fs::write(
        outer.join("attributes.crn"),
        "attributes { loaded: Bool = true }\n",
    )
    .expect("write outer attribute");
    std::fs::write(producer.join("arguments.crn"), "arguments { value: Int }\n")
        .expect("write producer arguments");
    std::fs::write(
        producer.join("attributes.crn"),
        "attributes { forwarded = value }\n",
    )
    .expect("write producer attribute");
    std::fs::write(inner.join("arguments.crn"), "arguments { value: String }\n")
        .expect("write inner arguments");

    let schemas = SchemaRegistry::new();
    assert_module_resolution_expands(&outer, &schemas);
    let cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(schemas),
        &fixture,
        "modules/outer/calls.crn",
    );
    let message = "module call 'inner (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from produced.forwarded)";
    let expected = canonical_error_multiset([message.to_string()]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(lsp_error_multiset(&lsp), expected, "LSP={lsp:#?}");
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing nested binding mismatch: {lsp:#?}"));
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(2, "inner { value = ".len() as u32),
            tower_lsp::lsp_types::Position::new(
                2,
                ("inner { value = ".len() + "forwarded".len()) as u32,
            ),
        ),
        "the typed binding mismatch must underline the bare value use",
    );
}

#[test]
fn for_expanded_root_bare_binding_reference_mismatch_has_exact_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("expanded binding-reference parity fixture");
    let producer = fixture.path().join("modules/producer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&producer).expect("create producer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("imports.crn"),
        r#"let producer = use { source = "./modules/producer" }
let inner = use { source = "./modules/inner" }
"#,
    )
    .expect("write root imports");
    std::fs::write(producer.join("arguments.crn"), "arguments { value: Int }\n")
        .expect("write producer arguments");
    std::fs::write(
        producer.join("attributes.crn"),
        "attributes { forwarded = value }\n",
    )
    .expect("write producer attribute");
    let calls = r#"let produced = producer { value = 7 }
let forwarded = produced.forwarded
for item in ["one"] {
  inner { value = forwarded }
}
"#;
    std::fs::write(fixture.path().join("calls.crn"), calls).expect("write expanded call");
    std::fs::write(inner.join("arguments.crn"), "arguments { value: String }\n")
        .expect("write inner arguments");

    let schemas = SchemaRegistry::new();
    assert_module_resolution_expands(fixture.path(), &schemas);
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "calls.crn");
    let message = "module call 'inner (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from produced.forwarded)";
    let expected = canonical_error_multiset([message.to_string()]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(lsp_error_multiset(&lsp), expected, "LSP={lsp:#?}");
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing expanded binding mismatch: {lsp:#?}"));
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(3, "  inner { value = ".len() as u32),
            tower_lsp::lsp_types::Position::new(
                3,
                ("  inner { value = ".len() + "forwarded".len()) as u32,
            ),
        ),
        "the expanded typed mismatch must keep the authored bare-use range",
    );
}

#[test]
fn mixed_module_call_and_export_reference_errors_are_each_reported_once() {
    let fixture = tempfile::tempdir().expect("mixed reference-error fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("values.crn"),
        r#"provider test {}
let source = test.r.Source {
  name = "source"
  count = 7
}
"#,
    )
    .expect("write sibling values");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }

inner { value = source.count }

exports {
  bad: Int = source.name
}
"#,
    )
    .expect("write root call and export");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { value: String }\n",
    )
    .expect("write module signature");

    let schemas = module_call_parity_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let call_error = "argument 'value': cannot assign Int to 'value': expected String, got Int (from source.count)";
    let export_error = "export 'bad': type mismatch for 'source.name': expected Int, got String";

    assert_eq!(
        cli_messages_count(&cli, call_error),
        1,
        "CLI must not repeat the located module-call error in the residual block: {cli:#?}",
    );
    assert_eq!(
        cli_messages_count(&cli, export_error),
        1,
        "CLI must retain the uncovered export error exactly once: {cli:#?}",
    );
    assert_eq!(
        lsp_messages_count(&lsp, call_error),
        1,
        "LSP must retain the located module-call error exactly once: {lsp:#?}",
    );
    assert_eq!(
        lsp_messages_count(&lsp, export_error),
        1,
        "LSP must retain the uncovered export error exactly once: {lsp:#?}",
    );
}

#[test]
fn unannotated_forwarded_module_output_mismatch_has_cli_lsp_error_parity() {
    let fixture = tempfile::tempdir().expect("forwarded-output parity fixture");
    let outer = fixture.path().join("outer");
    let inner = fixture.path().join("inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let outer = use { source = "./outer" }

let outer_instance = outer {}
"#,
    )
    .expect("write root call");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = "../inner" }

let inner_instance = inner { value = "forwarded" }

attributes {
  bad: Int = inner_instance.forwarded
}
"#,
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        r#"arguments { value: String }

attributes {
  forwarded = value
}
"#,
    )
    .expect("write inner module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let cli_mismatches = cli
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("attribute 'bad': type mismatch")
                && diagnostic.contains("expected Int")
                && diagnostic.contains("got String")
        })
        .collect::<Vec<_>>();
    let lsp_mismatches = lsp
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("attribute 'bad': type mismatch")
                && diagnostic.message.contains("expected Int")
                && diagnostic.message.contains("got String")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        cli_mismatches.len(),
        1,
        "CLI must retain the expanded output-declaration failure: {cli:#?}",
    );
    assert_eq!(
        lsp_mismatches.len(),
        1,
        "LSP must retain the same expanded output-declaration failure: {lsp:#?}",
    );
    assert_eq!(
        lsp_mismatches[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    let cli_message = cli_located_message_multiset(&cli)
        .into_keys()
        .find(|message| message.contains("attribute 'bad': type mismatch"))
        .expect("canonical CLI mismatch message");
    assert_eq!(
        cli_message, lsp_mismatches[0].message,
        "CLI and LSP must render the core finding identically",
    );
}

#[test]
fn uncalled_imported_module_internal_call_has_cli_lsp_error_parity() {
    let fixture = tempfile::tempdir().expect("uncalled-module parity fixture");
    let outer = fixture.path().join("outer");
    let inner = fixture.path().join("inner");
    let source = fixture.path().join("source");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::create_dir_all(&source).expect("create source module");
    std::fs::write(
        fixture.path().join("main.crn"),
        "let unused_outer = use { source = \"./outer\" }\n",
    )
    .expect("write uncalled root import");
    let outer_source = r#"
let inner = use { source = "../inner" }
let source = use { source = "../source" }

let source_instance = source {}
inner { value = source_instance.value }
attributes { loaded: Bool = true }
"#;
    std::fs::write(outer.join("main.crn"), outer_source).expect("write outer module");
    std::fs::write(inner.join("main.crn"), "arguments { value: String }\n")
        .expect("write inner module");
    std::fs::write(source.join("main.crn"), "attributes { value: Int = 1 }\n")
        .expect("write source module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "outer/main.crn",
    );
    let lsp_errors = lsp_error_multiset(&lsp);
    let expected = CanonicalDiagnostic {
        message: "module call 'inner (anonymous call)': argument 'value': cannot assign Int to 'value': expected String, got Int (from source_instance.value)".to_string(),
        severity: "ERROR",
    };

    assert_eq!(
        lsp_errors.get(&expected),
        Some(&1),
        "the authored internal call must be reported once: {lsp:#?}",
    );
    assert_eq!(
        cli_located_error_multiset(&cli).get(&expected),
        Some(&1),
        "CLI must walk an imported-but-uncalled module and render the same core finding: {cli:#?}",
    );
}

#[test]
fn nested_call_below_uncalled_import_routes_to_the_root_call_with_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("nested uncalled-import parity fixture");
    let outer = fixture.path().join("outer");
    let inner = fixture.path().join("inner");
    let leaf = fixture.path().join("leaf");
    for directory in [&outer, &inner, &leaf] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let root_source = r#"let outer = use { source = "./outer" }

outer {}
"#;
    std::fs::write(fixture.path().join("main.crn"), root_source).expect("write root call fixture");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = "../inner" }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        r#"let leaf = use { source = "../leaf" }

leaf { bogus = 1 }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write inner module");
    std::fs::write(leaf.join("main.crn"), "arguments { expected: Int = 0 }\n")
        .expect("write leaf module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let expected =
        canonical_error_multiset([String::from("Unknown argument 'bogus' for module 'leaf'")]);

    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must retain the nested call failure exactly once: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must route the same nested failure to the root call: {lsp:#?}",
    );
    assert_eq!(
        lsp[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(2, 0),
            tower_lsp::lsp_types::Position::new(2, "outer {}".len() as u32),
        ),
        "the nested finding must underline the authored root call",
    );
}

#[test]
fn nested_call_below_root_uncalled_import_routes_to_the_use_statement() {
    let fixture = tempfile::tempdir().expect("root uncalled-import parity fixture");
    let outer = fixture.path().join("outer");
    let leaf = fixture.path().join("leaf");
    for directory in [&outer, &leaf] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let root_source = "let outer = use { source = \"./outer\" }\n";
    std::fs::write(fixture.path().join("main.crn"), root_source)
        .expect("write root import fixture");
    std::fs::write(
        outer.join("main.crn"),
        r#"let leaf = use { source = "../leaf" }

leaf { bogus = 1 }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write outer module");
    std::fs::write(leaf.join("main.crn"), "arguments { expected: Int = 0 }\n")
        .expect("write leaf module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let expected =
        canonical_error_multiset([String::from("Unknown argument 'bogus' for module 'leaf'")]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(lsp_error_multiset(&lsp), expected, "LSP={lsp:#?}");
    assert_eq!(
        lsp[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );
    assert_eq!(
        lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 0),
            tower_lsp::lsp_types::Position::new(
                0,
                root_source.trim_end_matches('\n').chars().count() as u32,
            ),
        ),
        "an uncalled root import must retain an authored editor anchor",
    );
}

#[test]
fn uncalled_import_type_resolution_error_has_cli_lsp_parity_and_declaration_range() {
    let fixture = tempfile::tempdir().expect("uncalled type-resolution parity fixture");
    let invalid = fixture.path().join("invalid");
    std::fs::create_dir_all(&invalid).expect("create invalid module directory");
    let root_source = "let invalid = use { source = \"./invalid\" }\n";
    let declaration_source = "arguments {\n  x: NoSuchType\n}\n";
    std::fs::write(fixture.path().join("main.crn"), root_source).expect("write uncalled import");
    std::fs::write(invalid.join("arguments.crn"), declaration_source)
        .expect("write invalid argument declaration");

    let schemas = module_call_parity_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let engine = engine_with_schemas(schemas);
    let root_lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
    let declaration_lsp = lsp_diagnostics(&engine, &fixture, "invalid/arguments.crn");
    let expected = canonical_error_multiset([String::from(
        "argument 'x': unknown custom type 'NoSuchType'",
    )]);

    assert_eq!(cli_located_error_multiset(&cli), expected, "CLI={cli:#?}");
    assert_eq!(
        lsp_error_multiset(&root_lsp),
        expected,
        "root LSP must route the declaration failure to the uncalled use: {root_lsp:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&declaration_lsp),
        expected,
        "module-file LSP must render the same typed failure: {declaration_lsp:#?}",
    );
    assert_eq!(
        root_lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 0),
            tower_lsp::lsp_types::Position::new(
                0,
                root_source.trim_end_matches('\n').chars().count() as u32,
            ),
        ),
    );
    assert_eq!(
        declaration_lsp[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 5),
            tower_lsp::lsp_types::Position::new(1, 15),
        ),
        "the authored parameter type, not an unrelated module call, must be underlined",
    );
}

#[test]
fn standalone_module_type_resolution_keeps_cli_location_and_lsp_range() {
    let fixture = tempfile::tempdir().expect("standalone type-resolution parity fixture");
    let module = fixture.path().join("module");
    std::fs::create_dir_all(&module).expect("create standalone module");
    let declaration_source = "arguments {\n  x: NoSuchType\n}\n";
    std::fs::write(module.join("arguments.crn"), declaration_source)
        .expect("write module arguments");
    std::fs::write(
        module.join("attributes.crn"),
        "attributes {\n  loaded: Bool = true\n}\n",
    )
    .expect("write module attributes");

    let schemas = module_call_parity_schemas();
    let strict_context = carina_core::parser::ProviderContext {
        environment: carina_core::EnvironmentSnapshot::empty(),
        decryptor: None,
        custom_types: carina_core::provider::provider_custom_types_from_factories(Arc::new(
            factories_for(&schemas),
        )),
        resource_types: carina_core::parser::ProviderContext::resource_types_from_schema_registry(
            &schemas,
        ),
    };
    let directory_failure =
        match carina_core::config_loader::parse_directory_with_overrides_and_diagnostics(
            &carina_core::paths::ResolvedPath::new(&module).unwrap(),
            &strict_context,
            &HashMap::new(),
        ) {
            Ok(_result) => panic!("the enriched directory parse must reject NoSuchType"),
            Err(failure) => failure,
        };
    assert_eq!(
        directory_failure.source_file(),
        Some(module.join("arguments.crn").as_path()),
    );
    assert!(directory_failure.parse_error().is_some_and(|error| {
        matches!(
            error,
            carina_core::parser::ParseError::UnresolvedDeclarationType {
                token,
                line,
                message,
            } if *line == 2
                && token.name() == "NoSuchType"
                && message == "unknown custom type 'NoSuchType'"
        )
    }));

    let cli = cli_diagnostics_at(factories_for(&schemas), &module);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(schemas),
        &fixture,
        "module/arguments.crn",
    );
    let message = "argument 'x': unknown custom type 'NoSuchType'";
    let expected = canonical_error_multiset([message.to_string()]);
    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must replace the parser backstop with one contextual error: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must replace the parser backstop with the same contextual ERROR: {lsp:#?}",
    );
    let cli_diagnostic = cli
        .iter()
        .find(|diagnostic| diagnostic.contains(message))
        .unwrap_or_else(|| panic!("missing CLI type-resolution diagnostic: {cli:#?}"));
    let lsp_diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing LSP type-resolution diagnostic: {lsp:#?}"));
    assert_eq!(
        lsp_diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
    );

    assert!(
        cli_diagnostic.contains("module/arguments.crn at line 2\n"),
        "CLI must retain the declaration file and one-based line: {cli:#?}",
    );
    assert_eq!(
        lsp_diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 5),
            tower_lsp::lsp_types::Position::new(1, 15),
        ),
        "LSP must underline the same line-two declaration named by the CLI",
    );
}

#[test]
fn nested_constraint_keeps_cli_location_and_lsp_owner_range() {
    let fixture = tempfile::tempdir().expect("nested located-constraint parity fixture");
    let outer = fixture.path().join("outer");
    let checked = fixture.path().join("checked");
    for directory in [&outer, &checked] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let root_source = r#"let outer = use { source = "./outer" }

let instance = outer {}
"#;
    let outer_source = r#"let checked = use { source = "../checked" }

checked {
  name = ""
}

attributes { loaded: Bool = true }
"#;
    std::fs::write(fixture.path().join("main.crn"), root_source).expect("write root call");
    std::fs::write(outer.join("main.crn"), outer_source).expect("write nested call");
    std::fs::write(
        checked.join("arguments.crn"),
        r#"arguments {
  name: String {
    validation {
      condition = length(name) > 0
      error_message = "nested name must not be empty"
    }
  }
}
"#,
    )
    .expect("write checked arguments");
    std::fs::write(
        checked.join("attributes.crn"),
        "attributes {\n  name = name\n}\n",
    )
    .expect("write checked attributes");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let engine = engine_with_schemas(SchemaRegistry::new());
    let root_lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
    let nested_lsp = lsp_diagnostics(&engine, &fixture, "outer/main.crn");
    let message = "anonymous call to module 'checked': argument 'name': nested name must not be empty (got '')";
    let cli_diagnostic = cli
        .iter()
        .find(|diagnostic| diagnostic.contains(message))
        .unwrap_or_else(|| panic!("missing CLI nested constraint: {cli:#?}"));
    let root_diagnostic = root_lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing routed root LSP constraint: {root_lsp:#?}"));
    let nested_diagnostic = nested_lsp
        .iter()
        .find(|diagnostic| diagnostic.message == message)
        .unwrap_or_else(|| panic!("missing nested-file LSP constraint: {nested_lsp:#?}"));

    assert!(
        cli_diagnostic.contains("outer/main.crn at line 4\n"),
        "CLI must retain the immediate nested argument location: {cli:#?}",
    );
    assert_eq!(
        nested_diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(3, 9),
            tower_lsp::lsp_types::Position::new(3, 11),
        ),
        "the CLI line-four anchor and nested LSP range must name the same argument",
    );
    assert_eq!(
        root_diagnostic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(2, 15),
            tower_lsp::lsp_types::Position::new(2, 23),
        ),
        "the typed finding must retain its root owner for editor routing",
    );
}

#[test]
fn constraint_and_later_unknown_argument_are_collected_by_cli_and_lsp() {
    let fixture = tempfile::tempdir().expect("collect-all module-call parity fixture");
    let checked = fixture.path().join("checked");
    let other = fixture.path().join("other");
    let leaf = fixture.path().join("leaf");
    for directory in [&checked, &other, &leaf] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    std::fs::write(
        fixture.path().join("imports.crn"),
        r#"let checked = use { source = "./checked" }
let other = use { source = "./other" }
"#,
    )
    .expect("write root imports");
    let calls_source = r#"checked {
  name = ""
}

other {
}
"#;
    std::fs::write(fixture.path().join("calls.crn"), calls_source).expect("write root calls");
    std::fs::write(
        checked.join("arguments.crn"),
        r#"arguments {
  name: String {
    validation {
      condition = length(name) > 0
      error_message = "name must not be empty"
    }
  }
}
"#,
    )
    .expect("write checked arguments");
    std::fs::write(
        checked.join("attributes.crn"),
        "attributes {\n  name = name\n}\n",
    )
    .expect("write checked attributes");
    std::fs::write(
        other.join("arguments.crn"),
        "attributes {\n  loaded: Bool = true\n}\n",
    )
    .expect("write other attributes");
    std::fs::write(
        other.join("imports.crn"),
        "let leaf = use { source = '../leaf' }\n",
    )
    .expect("write nested import");
    std::fs::write(
        other.join("calls.crn"),
        "leaf {\n  known = true\n  bogus = true\n}\n",
    )
    .expect("write nested invalid call");
    std::fs::write(
        leaf.join("arguments.crn"),
        "arguments {\n  known: Bool\n}\n",
    )
    .expect("write leaf arguments");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "calls.crn",
    );
    let constraint =
        "anonymous call to module 'checked': argument 'name': name must not be empty (got '')";
    let unknown = "Unknown argument 'bogus' for module 'leaf'";
    let expected = canonical_error_multiset([constraint.to_string(), unknown.to_string()]);

    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must report the constraint and later resolver failure together: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must report the same two typed findings: {lsp:#?}",
    );
    let cli_unknown = cli
        .iter()
        .find(|diagnostic| diagnostic.contains(unknown))
        .unwrap_or_else(|| panic!("missing CLI nested unknown argument: {cli:#?}"));
    assert!(
        cli_unknown.contains("other/calls.crn at line 3\n"),
        "CLI must retain the nested argument anchor: {cli:#?}",
    );
    for (message, range) in [
        (
            constraint,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(1, 9),
                tower_lsp::lsp_types::Position::new(1, 11),
            ),
        ),
        (
            unknown,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(4, 0),
                tower_lsp::lsp_types::Position::new(5, 1),
            ),
        ),
    ] {
        let diagnostic = lsp
            .iter()
            .find(|diagnostic| diagnostic.message == message)
            .unwrap_or_else(|| panic!("missing LSP diagnostic {message:?}: {lsp:#?}"));
        assert_eq!(diagnostic.range, range);
    }
}

#[test]
fn nested_constraint_failure_routes_each_instance_message_to_its_root_call() {
    let fixture = tempfile::tempdir().expect("nested repeated-constraint parity fixture");
    let outer = fixture.path().join("outer");
    let checked = fixture.path().join("checked");
    for directory in [&outer, &checked] {
        std::fs::create_dir_all(directory).expect("create module directory");
    }
    let first_line = "let a = outer { name = \"first-too-long\" }";
    let second_line = "let b = outer { name = \"second-too-long\" }";
    let root_source =
        format!("let outer = use {{ source = \"./outer\" }}\n\n{first_line}\n{second_line}\n");
    std::fs::write(fixture.path().join("main.crn"), &root_source)
        .expect("write repeated root calls");
    std::fs::write(
        outer.join("main.crn"),
        r#"arguments { name: String }

let checked = use { source = "../checked" }
checked { name = name }

attributes { loaded: Bool = true }
"#,
    )
    .expect("write outer module");
    std::fs::write(
        checked.join("main.crn"),
        r#"arguments {
  name: String {
    validation {
      condition = length(name) <= 3
      error_message = "name is too long"
    }
  }
}
"#,
    )
    .expect("write checked module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let first_message = "anonymous call to module 'checked': argument 'name': name is too long (got 'first-too-long')";
    let second_message = "anonymous call to module 'checked': argument 'name': name is too long (got 'second-too-long')";
    let expected =
        canonical_error_multiset([first_message.to_string(), second_message.to_string()]);

    assert_eq!(
        cli_located_error_multiset(&cli),
        expected,
        "CLI must retain both concrete constraint outcomes: {cli:#?}",
    );
    assert_eq!(
        lsp_error_multiset(&lsp),
        expected,
        "LSP must retain both concrete constraint outcomes: {lsp:#?}",
    );
    for (message, line, source_line) in [
        (first_message, 2, first_line),
        (second_message, 3, second_line),
    ] {
        let diagnostic = lsp
            .iter()
            .find(|diagnostic| diagnostic.message == message)
            .unwrap_or_else(|| panic!("missing per-instance diagnostic {message:?}: {lsp:#?}"));
        assert_eq!(
            diagnostic.severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        );
        assert_eq!(
            diagnostic.range,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(line, "let a = ".len() as u32),
                tower_lsp::lsp_types::Position::new(line, source_line.len() as u32),
            ),
            "each concrete constraint message must underline its own root call",
        );
    }
}

#[test]
fn ignore_changes_validation_has_cli_lsp_parity_for_every_v1_error() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        (
            "backend.crn",
            "backend local { path = \"carina.state.json\" }\n",
        ),
        (
            "main.crn",
            r#"let missing = mock.test.resource {
  name = "missing"
  directives { ignore_changes = [does_not_exist] }
}
let read_only = mock.test.resource {
  name = "read-only"
  directives { ignore_changes = [identifier] }
}
let indexed = mock.test.resource {
  name = "indexed"
  rule { action = "allow" }
  directives { ignore_changes = [rule[0].action] }
}
let through_list = mock.test.resource {
  name = "through-list"
  rule { action = "allow" }
  directives { ignore_changes = [rule.action] }
}
let union_path = mock.test.resource {
  name = "union"
  choice = { value = "selected" }
  directives { ignore_changes = [choice.value] }
}
let unwritten = mock.test.resource {
  name = "unwritten"
  directives { ignore_changes = [comment] }
}
let duplicate = mock.test.resource {
  name = "duplicate"
  comment = "recorded"
  directives { ignore_changes = [comment, comment] }
}
"#,
        ),
    ]);
    let schemas = carina_provider_mock::builtin_schemas().into_iter().fold(
        SchemaRegistry::new(),
        |mut registry, schema| {
            registry.insert("mock", schema);
            registry
        },
    );
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let extract = |message: &str| {
        message
            .find("ignore_changes path")
            .map(|start| message[start..].to_string())
    };
    let mut cli_messages = cli
        .iter()
        .filter_map(|message| extract(message))
        .collect::<Vec<_>>();
    let mut lsp_messages = lsp
        .iter()
        .filter_map(|diagnostic| extract(&diagnostic.message))
        .collect::<Vec<_>>();
    cli_messages.sort();
    lsp_messages.sort();

    assert_eq!(cli_messages, lsp_messages, "CLI={cli:#?}; LSP={lsp:#?}");
    assert_eq!(
        cli_messages,
        [
            "ignore_changes path `choice.value` passes through a union",
            "ignore_changes path `comment` is listed more than once",
            "ignore_changes path `comment` is not written in this resource block",
            "ignore_changes path `does_not_exist` does not exist in the resource schema",
            "ignore_changes path `identifier` reaches a read-only or provider-populated node",
            "ignore_changes path `rule.action` passes through a list, which is not supported",
            "ignore_changes path `rule[0].action` contains a list index, which is not supported",
        ]
    );
}

#[test]
fn unbound_deferred_template_type_error_keeps_authored_source_location() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"provider mock {}
let src = mock.test.resource { name = "src" }
let n = 5

for _, t in (src.tags ?? {}) {
  mock.test.resource { name = n }
}
"#,
    )]);
    let schemas = carina_provider_mock::builtin_schemas().into_iter().fold(
        SchemaRegistry::new(),
        |mut registry, schema| {
            registry.insert("mock", schema);
            registry
        },
    );

    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    let mismatch = diagnostics
        .iter()
        .find(|message| message.contains("Type mismatch: expected String, got Int"))
        .unwrap_or_else(|| panic!("missing deferred-template type error: {diagnostics:#?}"));
    assert!(
        mismatch.contains("main.crn at line 6\nType mismatch: expected String, got Int"),
        "the template must retain the authored source lookup: {mismatch}",
    );
    assert!(
        !mismatch.contains("<pending>"),
        "a stable parse-time template id must prevent pending-id fallback: {mismatch}",
    );
}

#[test]
fn postfix_wrong_shape_access_has_cli_lsp_parity_for_local_and_wait_bindings() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        (
            "values.crn",
            r#"let r0 = mock.test.resource { name = "z" }
let q = mock.test.resource {
  name = "q-z"
  comment = "comment"
}
let forwarded = mock.test.resource { name = r0.identifier }
let qw = wait q {
  until = q.name == "q-z"
  timeout = 1min
}
"#,
        ),
        (
            "main.crn",
            r#"let literal = mock.test.resource { name = q.name[0] }
let provider_populated = mock.test.resource { name = q.identifier[0] }
let waited = mock.test.resource { name = qw.name["key"] }
let forwarded_value = mock.test.resource { name = forwarded.name[1] }
let optional = mock.test.resource { name = q.comment[2] }
let valid = mock.test.resource { name = q.comment }
"#,
        ),
    ]);
    let schemas = carina_provider_mock::builtin_schemas().into_iter().fold(
        SchemaRegistry::new(),
        |mut registry, schema| {
            registry.insert("mock", schema);
            registry
        },
    );

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let mut cli_access_errors = cli
        .iter()
        .filter_map(|message| {
            message
                .find("cannot apply access")
                .map(|start| message[start..].to_string())
        })
        .collect::<Vec<_>>();
    let mut lsp_access_errors = lsp
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("cannot apply access"))
        .map(|diagnostic| diagnostic.message.clone())
        .collect::<Vec<_>>();
    cli_access_errors.sort();
    lsp_access_errors.sort();

    assert_eq!(
        cli_access_errors, lsp_access_errors,
        "CLI validate and LSP must report the same reference-path access errors; \
         CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert_eq!(
        cli_access_errors,
        [
            "cannot apply access ['key'] to String",
            "cannot apply access [0] to String",
            "cannot apply access [0] to String",
            "cannot apply access [1] to String",
            "cannot apply access [2] to String",
        ],
        "every statically String-valued path must be rejected while the no-suffix control stays valid",
    );
}

#[test]
fn cli_module_call_uses_argument_type_declared_in_sibling_file() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&inner).expect("create inner module");
    std::fs::write(
        fixture.path().join("arguments.crn"),
        "arguments {\n  count: Int\n}\n",
    )
    .expect("write directory arguments");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = './modules/inner' }

inner {
  name = count
}
"#,
    )
    .expect("write module call");
    std::fs::write(inner.join("main.crn"), "arguments {\n  name: String\n}\n")
        .expect("write inner module");

    let diagnostics = cli_diagnostics(Vec::new(), &fixture);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains(
                "module call 'inner (anonymous call)': argument 'name': cannot assign Int to 'name': expected String, got Int (from count)",
            ) && diagnostic.contains("main.crn")
                && diagnostic.contains("line 4")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mismatches.len(),
        1,
        "BindingIndex must report the merged directory argument type once: {diagnostics:#?}"
    );
}

#[test]
fn nested_string_argument_to_literal_union_is_accepted_consistently() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("modules/outer");
    let inner = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&outer).expect("create outer module");
    std::fs::create_dir_all(&inner).expect("create inner module");
    let root = r#"let outer = use { source = './modules/outer' }

outer {
  outer_env = 'dev'
}
"#;
    std::fs::write(fixture.path().join("main.crn"), root).expect("write root");
    std::fs::write(
        outer.join("main.crn"),
        r#"let inner = use { source = '../inner' }

arguments {
  outer_env: String
}

inner {
  env = outer_env
}
"#,
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        "arguments {\n  env: 'dev' | 'prod'\n}\n",
    )
    .expect("write inner module");

    // #2648 lifts the literal-only union to plain String, so a declared String
    // source remains assignable in both CLI and LSP validation.
    let cli = cli_diagnostics(Vec::new(), &fixture);
    assert!(
        cli.iter()
            .all(|message| !message.contains("Invalid argument type")),
        "CLI validation must use schema assignability policy: {cli:#?}"
    );
    let outer_path = outer.join("main.crn");
    let outer_text = std::fs::read_to_string(&outer_path).expect("read outer module");
    let outer_doc = Document::new_with_path(
        outer_text,
        outer_path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let lsp = engine_with_schemas(SchemaRegistry::new()).analyze(&outer_doc, Some(&outer_path));
    assert!(
        lsp.iter()
            .all(|diagnostic| !diagnostic.message.contains("Invalid argument type")),
        "LSP and CLI must agree on the String boundary: {lsp:#?}"
    );
}

#[test]
fn nested_int_argument_to_literal_union_is_rejected_by_cli_and_lsp() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("modules/outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("create nested modules");
    std::fs::write(
        fixture.path().join("main.crn"),
        "let outer = use { source = './modules/outer' }\nouter {\n  n = 1\n}\n",
    )
    .expect("write root");
    std::fs::write(
        outer.join("main.crn"),
        "let inner = use { source = './inner' }\narguments {\n  n: Int\n}\ninner {\n  env = n\n}\n",
    )
    .expect("write outer module");
    std::fs::write(
        inner.join("main.crn"),
        "arguments {\n  env: 'dev' | 'prod'\n}\n",
    )
    .expect("write inner module");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    assert!(
        cli.iter().any(|message| {
            message.contains("Type mismatch: expected 'dev' | 'prod', got Int (from n)")
        }),
        "CLI BindingIndex must reject Int forwarded to a literal-string union: {cli:#?}"
    );

    let outer_path = outer.join("main.crn");
    let outer_text = std::fs::read_to_string(&outer_path).expect("read outer module");
    let outer_doc = Document::new_with_path(
        outer_text,
        outer_path.clone(),
        Arc::new(carina_core::parser::ProviderContext::new(
            carina_core::EnvironmentSnapshot::empty(),
        )),
    );
    let lsp = engine_with_schemas(SchemaRegistry::new()).analyze(&outer_doc, Some(&outer_path));
    assert!(
        lsp.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("Type mismatch: expected 'dev' | 'prod', got Int (from n)")
        }),
        "LSP BindingIndex must reject the same nested forwarding: {lsp:#?}"
    );
}

#[test]
fn resource_handle_forwarding_is_checked_in_root_module_and_lsp() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("nested module directories");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"provider mock {}
let r = mock.test.resource { name = 'r' }
let outer = use { source = './outer' }
outer { a = r }
"#,
    )
    .expect("root configuration");
    let outer_source = r#"let inner = use { source = './inner' }
arguments { a: mock.test.resource }
inner { x = a }
"#;
    std::fs::write(outer.join("main.crn"), outer_source).expect("outer module");
    std::fs::write(inner.join("main.crn"), "arguments { x: Int }\n").expect("inner module");

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let expected = "expected Int, got mock.test.resource (from a)";

    let root_cli = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli_messages_contain(&root_cli, expected),
        "root validation must reject Ref -> Int forwarding: {root_cli:#?}"
    );
    let module_cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    assert!(
        module_cli.iter().any(|message| {
            message.contains("expected Int, got mock.test.resource (from a)")
                && message.contains("main.crn")
                && message.contains("line 3")
        }),
        "validating the module directory must reject Ref -> Int forwarding: {module_cli:#?}"
    );
    let outer_path = outer.join("main.crn");
    let lsp = engine_with_schemas(schemas.clone()).analyze(
        &Document::new_with_path(
            outer_source.to_string(),
            outer_path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        ),
        Some(&outer_path),
    );
    assert!(
        lsp.iter().any(|diagnostic| {
            diagnostic
                .message
                .contains("'a' is a resource handle, not a value")
        }),
        "LSP must reject Ref -> Int forwarding: {lsp:#?}"
    );

    std::fs::write(inner.join("main.crn"), "arguments { x: String }\n")
        .expect("String sink module");
    let root_cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let module_cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    let lsp = engine_with_schemas(schemas).analyze(
        &Document::new_with_path(
            outer_source.to_string(),
            outer_path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        ),
        Some(&outer_path),
    );
    let bare_handle = "'a' is a resource handle, not a value";
    let typed_mismatch = "expected String, got mock.test.resource (from a)";
    assert!(
        root_cli
            .iter()
            .any(|message| { message.contains(bare_handle) || message.contains(typed_mismatch) })
            && module_cli
                .iter()
                .any(|message| message.contains(typed_mismatch))
            && lsp
                .iter()
                .any(|diagnostic| diagnostic.message.contains(bare_handle)),
        "a resource handle must not flow into a String parameter; root={root_cli:#?}, module={module_cli:#?}, lsp={lsp:#?}"
    );
}

#[test]
fn nested_module_accepts_resource_typed_argument_through_composition_signature() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let outer = fixture.path().join("outer");
    let inner = outer.join("inner");
    std::fs::create_dir_all(&inner).expect("nested module directories");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"provider mock {}
let resource = mock.test.resource { name = 'r' }
let outer = use { source = './outer' }
outer { resource = resource }
"#,
    )
    .expect("root configuration");
    let outer_source = r#"let inner = use { source = './inner' }
arguments { resource: mock.test.resource }
inner { resource = resource }
"#;
    std::fs::write(outer.join("main.crn"), outer_source).expect("outer module");
    std::fs::write(
        inner.join("main.crn"),
        "arguments { resource: mock.test.resource }\n",
    )
    .expect("inner module");

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let root_cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let module_cli = cli_diagnostics_at(factories_for(&schemas), &outer);
    let outer_path = outer.join("main.crn");
    let lsp = engine_with_schemas(schemas).analyze(
        &Document::new_with_path(
            outer_source.to_string(),
            outer_path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        ),
        Some(&outer_path),
    );

    assert!(
        root_cli.is_empty() && module_cli.is_empty() && lsp.is_empty(),
        "the nested composition signature must accept the resource handle; root={root_cli:#?}, module={module_cli:#?}, lsp={lsp:#?}",
    );
}

#[test]
fn cli_checks_reference_types_at_user_function_arguments_and_returns() {
    let fx1 = r#"provider mock {}
let r = mock.test.resource { name = 'r' }
fn f(x: Int) { 'fixed' }
mock.test.resource { name = f(r.name) }
"#;
    let fn14 = r#"provider mock {}
let r = mock.test.resource { name = 'r' }
fn f(): Int { r.name }
mock.test.resource { name = f() }
"#;
    let matching = r#"provider mock {}
let r = mock.test.resource { name = 'r' }
fn f(x: String): String { x }
mock.test.resource { name = f(r.name) }
"#;
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    for (name, source) in [("fx1", fx1), ("fn14", fn14)] {
        let fixture = write_fixture(&[("main.crn", source)]);
        let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
        assert!(
            diagnostics.iter().any(|message| {
                message.contains("function 'f'") && message.contains("expected Int")
            }),
            "{name} must reject the reference-valued function boundary: {diagnostics:#?}"
        );
    }

    let fixture = write_fixture(&[("main.crn", matching)]);
    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics
            .iter()
            .all(|message| !message.contains("function 'f'")),
        "matching function argument and return types must pass: {diagnostics:#?}"
    );
}

#[test]
fn cli_checks_nested_user_function_return_types_across_declaration_order_and_files() {
    let forward_declared = write_fixture(&[(
        "main.crn",
        r#"fn f(x: Int): String { 'ok' }
let result = f(g(1))
fn g(y: Int): String { 'str' }
"#,
    )]);
    let sibling_declared = write_fixture(&[
        (
            "main.crn",
            "fn f(x: Int): String { 'ok' }\nlet result = f(g(1))\n",
        ),
        ("funcs.crn", "fn g(y: Int): String { 'str' }\n"),
    ]);

    for (label, fixture) in [
        ("forward-declared", &forward_declared),
        ("sibling-declared", &sibling_declared),
    ] {
        let diagnostics = cli_diagnostics(Vec::new(), fixture);
        assert!(
            diagnostics.iter().any(|message| {
                message.contains("function 'f'")
                    && message.contains("expected Int")
                    && (message.contains("got String") || message.contains("got string"))
            }),
            "{label} g(): String must not satisfy f(Int): {diagnostics:#?}"
        );
        assert!(
            diagnostics
                .iter()
                .all(|message| !message.contains("\"str\"")),
            "evaluated nested-call contents must stay redacted: {diagnostics:#?}"
        );
    }

    let matching = write_fixture(&[
        (
            "main.crn",
            "fn f(x: Int): String { 'ok' }\nlet result = f(g(1))\n",
        ),
        ("funcs.crn", "fn g(y: Int): Int { y }\n"),
    ]);
    let diagnostics = cli_diagnostics(Vec::new(), &matching);
    assert!(
        diagnostics
            .iter()
            .all(|message| !message.contains("function 'f'")),
        "an Int-returning nested function must remain accepted: {diagnostics:#?}"
    );
}

#[test]
fn cli_rejects_concrete_attribute_literal_mismatch() {
    let fixture = write_fixture(&[("main.crn", "attributes {\n  y: 'dev' = 'prod'\n}\n")]);

    let diagnostics = cli_diagnostics(Vec::new(), &fixture);
    let mismatches = diagnostics
        .iter()
        .filter(|diagnostic| {
            diagnostic.contains("attribute 'y'")
                && diagnostic.contains("expected 'dev', got string 'prod'")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mismatches.len(),
        1,
        "carina validate must type-check concrete attributes values: {diagnostics:#?}"
    );
}

#[test]
fn module_validation_evaluation_error_is_reported_once_with_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("checked");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  x: String {
    validation {
      condition     = x > 5
      error_message = "x must be greater than five"
    }
  }
}
"#,
    )
    .expect("write module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let checked = use { source = './checked' }

let instance = checked {
  x = "abc"
}
"#,
    )
    .expect("write root configuration");

    let lsp_diags = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let cli_diags = cli_diagnostics(Vec::new(), &fixture);
    let lsp_constraint_messages = lsp_diags
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("x must be greater than five"))
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    let cli_constraint_messages = cli_located_message_multiset(&cli_diags)
        .into_iter()
        .filter(|(message, _count)| message.contains("x must be greater than five"))
        .collect::<BTreeMap<_, _>>();

    assert_eq!(
        lsp_constraint_messages.len(),
        1,
        "LSP must report the evaluator error once: {lsp_constraint_messages:#?}; all diagnostics: {lsp_diags:#?}"
    );
    assert_eq!(
        cli_constraint_messages.len(),
        1,
        "CLI must report the evaluator error once: {cli_constraint_messages:#?}; all diagnostics: {cli_diags:#?}"
    );
    assert_eq!(
        cli_constraint_messages.get(lsp_constraint_messages[0]),
        Some(&1),
    );
    assert_eq!(
        lsp_constraint_messages,
        [
            "module 'checked' (call 'instance'): argument 'x': x must be greater than five (got 'abc'); error evaluating constraint: cannot compare string with int"
        ]
    );
}

fn reference_valued_module_scope_fixture(module_source: &str) -> (TempDir, SchemaRegistry) {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("checked");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(module.join("main.crn"), module_source).expect("write module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"provider test {}

let checked = use { source = './checked' }

let name_source = test.r.producer {
  name = "name"
}

let peer_source = test.r.producer {
  name = "peer"
}

let instance = checked {
  name = name_source.id
  peer = peer_source.id
}
"#,
    )
    .expect("write root configuration");

    let producer = ResourceSchema::new("r.producer")
        .attribute(AttributeSchema::required("name", AttributeType::string()).build())
        .attribute(AttributeSchema::provider_populated("id", AttributeType::string()).build());
    (fixture, single_schema_map(producer))
}

fn assert_single_cli_lsp_constraint_scope_error(
    fixture: &TempDir,
    schemas: &SchemaRegistry,
    expected: &str,
) {
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(schemas), fixture);
    let lsp_matching = lsp_diags
        .iter()
        .filter(|diagnostic| diagnostic.message.contains(expected))
        .collect::<Vec<_>>();
    let cli_matching = cli_diags
        .iter()
        .filter(|diagnostic| diagnostic.contains(expected))
        .collect::<Vec<_>>();

    assert_eq!(
        lsp_matching.len(),
        1,
        "LSP must report the static constraint scope error once: {lsp_diags:#?}"
    );
    assert_eq!(
        cli_matching.len(),
        1,
        "CLI validate must report the static constraint scope error once: {cli_diags:#?}"
    );
}

#[test]
fn reference_valued_require_rejects_undeclared_variable_during_validation() {
    let (fixture, schemas) = reference_valued_module_scope_fixture(
        r#"arguments {
  name: String
  peer: String
}

require nmae != peer, "names must differ"
"#,
    );

    assert_single_cli_lsp_constraint_scope_error(
        &fixture,
        &schemas,
        "unknown variable 'nmae' in constraint expression",
    );
}

#[test]
fn reference_valued_argument_validation_rejects_sibling_variable_during_validation() {
    let (fixture, schemas) = reference_valued_module_scope_fixture(
        r#"arguments {
  name: String {
    validation {
      condition     = name != peer
      error_message = "names must differ"
    }
  }
  peer: String
}
"#,
    );

    assert_single_cli_lsp_constraint_scope_error(
        &fixture,
        &schemas,
        "unknown variable 'peer' in constraint expression",
    );
}

#[test]
fn anonymous_module_constraint_hides_synthetic_instance_with_cli_lsp_parity() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let module = fixture.path().join("checked");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  name: String {
    validation {
      condition     = length(name) > 0
      error_message = "name must not be empty"
    }
  }
}
"#,
    )
    .expect("write module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let checked = use { source = './checked' }

checked {
  name = ""
}
"#,
    )
    .expect("write root configuration");

    let lsp_diags = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    let cli_diags = cli_diagnostics(Vec::new(), &fixture);
    let lsp_messages = lsp_diags
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("name must not be empty"))
        .map(|diagnostic| diagnostic.message.as_str())
        .collect::<Vec<_>>();
    let cli_messages = cli_located_message_multiset(&cli_diags);

    assert_eq!(lsp_messages.len(), 1, "LSP diagnostics: {lsp_diags:#?}");
    assert_eq!(cli_messages.len(), 1, "CLI diagnostics: {cli_diags:#?}");
    assert_eq!(cli_messages.get(lsp_messages[0]), Some(&1));
    assert_eq!(
        lsp_messages,
        ["anonymous call to module 'checked': argument 'name': name must not be empty (got '')"]
    );
}

// NOTE: case-sensitive `contains`. LSP and CLI surfaces sometimes
// differ in casing (e.g. "Type mismatch" vs "type mismatch") because
// some diagnostics originate in carina-lsp and others in carina-core.
// When an assertion needs to bind to both surfaces uniformly use
// `lsp_messages_contain_ci` / `cli_messages_contain_ci` below — a
// case-sensitive miss against one surface silently makes the assertion
// half-empty.
fn lsp_messages_contain(diags: &[tower_lsp::lsp_types::Diagnostic], substring: &str) -> bool {
    diags.iter().any(|d| d.message.contains(substring))
}

fn cli_messages_contain(diags: &[String], substring: &str) -> bool {
    diags.iter().any(|m| m.contains(substring))
}

fn lsp_messages_count(diags: &[tower_lsp::lsp_types::Diagnostic], substring: &str) -> usize {
    diags
        .iter()
        .filter(|d| d.message.contains(substring))
        .count()
}

fn cli_messages_count(diags: &[String], substring: &str) -> usize {
    diags.iter().filter(|m| m.contains(substring)).count()
}

// LSP and CLI diagnostic wording differs in casing — LSP says
// "Type mismatch" (capital T) while CLI says "type mismatch". A
// case-sensitive `contains` against either spelling silently misses
// the other side. Use lowercase comparisons when the assertion needs
// to bind to both surfaces uniformly.
fn lsp_messages_contain_ci(diags: &[tower_lsp::lsp_types::Diagnostic], substring: &str) -> bool {
    let needle = substring.to_ascii_lowercase();
    diags
        .iter()
        .any(|d| d.message.to_ascii_lowercase().contains(&needle))
}

fn cli_messages_contain_ci(diags: &[String], substring: &str) -> bool {
    let needle = substring.to_ascii_lowercase();
    diags
        .iter()
        .any(|m| m.to_ascii_lowercase().contains(&needle))
}

// ============================================================================
// Test ProviderFactory — schema injection without WASM
// ============================================================================

struct TestProviderFactory {
    name: String,
    schemas: Vec<ValidatedResourceSchema>,
}

impl ProviderFactory for TestProviderFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        "Test provider"
    }

    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }

    fn create_provider(
        &self,
        _config: &carina_core::provider::ProviderSession,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<
        '_,
        carina_core::provider::ProviderResult<carina_core::provider::ProviderInitialization>,
    > {
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(NoopProvider)))
        })
    }

    fn create_normalizer(
        &self,
        _config: &carina_core::provider::ProviderSession,
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
        self.schemas.clone()
    }
}

struct NoopProvider;

impl Provider for NoopProvider {
    fn name(&self) -> &str {
        "noop"
    }
    fn read(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::ReadRequest,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<carina_core::provider::ReadOutcome>>
    {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn read_data_source(
        &self,
        _r: &carina_core::provider::ProviderReadyDataSource,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<State>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn create(
        &self,
        _id: &carina_core::resource::ResourceId,
        _request: carina_core::provider::CreateRequest,
        _attempt: carina_core::provider::CreateAttempt,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::CreateOutcome, carina_core::provider::CreateFailure>,
    > {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn resume_create(
        &self,
        _request: carina_core::provider::ResumeCreateRequest,
    ) -> BoxFuture<
        '_,
        Result<carina_core::provider::ResumeOutcome, carina_core::provider::ResumeFailure>,
    > {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn update(
        &self,
        _id: &carina_core::resource::ResourceId,
        _identifier: &carina_core::provider::AppliedIdentifier,
        _request: carina_core::provider::UpdateRequest,
        _attempt: carina_core::provider::UpdateAttempt,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<carina_core::provider::UpdateOutcome>>
    {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }
    fn delete(
        &self,
        _attempt: carina_core::provider::DeleteAttempt,
    ) -> BoxFuture<'_, carina_core::provider::ProviderResult<()>> {
        Box::pin(async { unimplemented!("e2e parity tests do not exercise apply") })
    }

    fn required_permissions(
        &self,
        _id: &carina_core::resource::ResourceId,
        _op: carina_core::effect::PlanOp,
    ) -> ProviderResult<Vec<String>> {
        Ok(Vec::new())
    }
}

// ============================================================================
// Scenario 1: Enum bare / TypeQualified / fully-qualified all pass
// ============================================================================

fn enum_schemas() -> SchemaRegistry {
    let mode = AttributeType::enum_(
        carina_core::schema::enum_identity("Mode", Some("test.r")),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );
    single_schema_map(
        ResourceSchema::new("r.mode_holder")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("mode", mode).build()),
    )
}

fn enum_shadow_schemas() -> SchemaRegistry {
    let event_type = AttributeType::enum_(
        carina_core::schema::enum_identity("EventType", Some("awscc.cloudfront.Distribution")),
        Some(vec![
            "ViewerRequest".to_string(),
            "OriginRequest".to_string(),
        ]),
        vec![
            ("ViewerRequest".to_string(), "viewer_request".to_string()),
            ("OriginRequest".to_string(), "origin_request".to_string()),
        ],
        None,
        None,
    );
    let behavior = AttributeType::list(AttributeType::struct_(
        "Behavior",
        vec![StructField::optional("event_type", event_type.clone()).build()],
    ));
    let flow_log_resource_type = AttributeType::enum_(
        carina_core::schema::enum_identity("ResourceType", Some("awscc.ec2.FlowLog")),
        Some(vec![
            "NetworkInterface".to_string(),
            "Subnet".to_string(),
            "VPC".to_string(),
        ]),
        vec![
            (
                "NetworkInterface".to_string(),
                "network_interface".to_string(),
            ),
            ("Subnet".to_string(), "subnet".to_string()),
            ("VPC".to_string(), "vpc".to_string()),
        ],
        None,
        None,
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        ResourceSchema::new("test.Source")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("count", AttributeType::int()).build())
            .validate()
            .expect("source schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("cloudfront.Distribution")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("event_type", event_type.clone()).build())
            .attribute(
                AttributeSchema::optional("behaviors", behavior)
                    .with_block_name("behavior")
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("event_types", AttributeType::list(event_type.clone()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional("event_type_map", AttributeType::map(event_type.clone()))
                    .build(),
            )
            .validate()
            .expect("distribution schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("cloudfront.RequiredDistribution")
            .attribute(AttributeSchema::required("event_type", event_type).build())
            .validate()
            .expect("required distribution schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        ResourceSchema::new("ec2.FlowLog")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("resource_type", flow_log_resource_type).build())
            .validate()
            .expect("flow-log schema fixture must be valid"),
    );
    schemas
}

fn bare_enum_shadow_schemas() -> SchemaRegistry {
    let mode = AttributeType::enum_(
        carina_core::schema::TypeIdentity::bare("Mode"),
        Some(vec!["fast".to_string(), "slow".to_string()]),
        vec![],
        None,
        None,
    );
    single_schema_map(
        ResourceSchema::new("r.mode_holder")
            .attribute(AttributeSchema::optional("mode", mode).build()),
    )
}

const ENUM_SHADOW_MESSAGE: &str = "`viewer_request` was interpreted as the `let` binding; if you meant the enum `EventType` value, write `awscc.cloudfront.Distribution.EventType.viewer_request`";

fn enum_shadow_fixture(use_site: &str) -> TempDir {
    write_fixture(&[
        (
            "bindings.crn",
            r#"let viewer_request = awscc.test.Source {
  name = "shadow"
}
"#,
        ),
        (
            "main.crn",
            &format!("awscc.cloudfront.Distribution {{\n  name = \"distribution\"\n{use_site}}}\n"),
        ),
    ])
}

fn enum_shadow_module_argument_fixture() -> TempDir {
    let fixture = tempfile::tempdir().expect("module enum-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        "let viewer_request = awscc.test.Source { name = \"shadow\" }\n",
    )
    .expect("write binding fixture");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }
inner {
  event_type = viewer_request
}
"#,
    )
    .expect("write module call fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_type: awscc.cloudfront.Distribution.EventType\n}\n",
    )
    .expect("write module signature");
    fixture
}

#[test]
fn enum_member_shadowed_by_sibling_resource_binding_reports_typed_error_in_every_resource_position()
{
    for (position, use_site) in [
        ("attribute", Some("  event_type = viewer_request\n")),
        (
            "nested block",
            Some("  behavior {\n    event_type = viewer_request\n  }\n"),
        ),
        ("list element", Some("  event_types = [viewer_request]\n")),
        (
            "map element",
            Some("  event_type_map = { default = viewer_request }\n"),
        ),
        ("module argument", None),
    ] {
        let schemas = enum_shadow_schemas();
        let fixture =
            use_site.map_or_else(enum_shadow_module_argument_fixture, enum_shadow_fixture);
        let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE)),
            "{position} must report the typed enum-shadow diagnostic, got: {diagnostics:#?}",
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.contains("write 'viewer_request'")),
            "the diagnostic must not advertise a quoted-string escape rejected by strict enum validation: {diagnostics:#?}",
        );
        let matching_lsp = lsp
            .iter()
            .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
            .collect::<Vec<_>>();
        assert_eq!(
            matching_lsp.len(),
            1,
            "{position} must report the same typed enum-shadow diagnostic in the LSP, got: {lsp:#?}",
        );
        assert_eq!(
            matching_lsp[0].severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
            "{position} blocks CLI validation and must therefore be an LSP error",
        );
    }
}

#[test]
fn structural_binding_without_matching_enum_member_keeps_existing_guidance() {
    let schemas = enum_shadow_schemas();
    for (attribute, declaration) in [
        (
            "event_type",
            "let not_a_member = awscc.test.Source { name = \"shadow\" }\n",
        ),
        (
            "name",
            "let not_a_member = awscc.test.Source { name = \"shadow\" }\n",
        ),
    ] {
        let fixture = write_fixture(&[
            ("bindings.crn", declaration),
            (
                "main.crn",
                &format!(
                    "awscc.cloudfront.Distribution {{\n  name = \"distribution\"\n  {attribute} = not_a_member\n}}\n"
                ),
            ),
        ]);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
        let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp_message = lsp.iter().find(|diagnostic| {
            diagnostic.message.contains(
                "'not_a_member' is a resource, not a value; reference one of its attributes",
            )
        });
        let cli_message = diagnostics.iter().find(|diagnostic| {
            diagnostic.contains(
                "'not_a_member' is a resource, not a value; reference one of its attributes",
            )
        });
        assert!(
            lsp_message.is_some() && cli_message.is_some(),
            "{attribute} must retain CLI/LSP structural-binding guidance; CLI={diagnostics:#?}; LSP={lsp:#?}",
        );
        assert!(
            cli_message
                .expect("checked above")
                .contains(&lsp_message.expect("checked above").message),
            "{attribute} guidance must render identically; CLI={diagnostics:#?}; LSP={lsp:#?}",
        );
        assert!(
            diagnostics
                .iter()
                .all(|diagnostic| !diagnostic.contains("`not_a_member` was interpreted")),
            "a non-member spelling must not become an enum-shadow error: {diagnostics:#?}",
        );
    }
}

#[test]
fn pending_structural_enum_use_suppresses_secondary_missing_required_attribute() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let viewer_request = awscc.test.Source { name = "shadow" }
awscc.cloudfront.RequiredDistribution {
  event_type = viewer_request
}
"#,
    )]);

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert_eq!(
        cli.iter()
            .filter(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE))
            .count(),
        1,
        "CLI must report the authored structural collision once: {cli:#?}",
    );
    assert_eq!(
        lsp.iter()
            .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
            .count(),
        1,
        "LSP must report the authored structural collision once: {lsp:#?}",
    );
    assert!(
        lsp.iter().all(|diagnostic| {
            !diagnostic.message.to_ascii_lowercase().contains("required")
                && !diagnostic.message.to_ascii_lowercase().contains("missing")
        }),
        "the typed pending slot must suppress the missing-attribute error: {lsp:#?}",
    );
}

#[test]
fn non_enum_value_binding_keeps_existing_reference_type_check() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let source = awscc.test.Source {
  name = "shadow"
  count = 1
}
let forwarded = source.count

awscc.cloudfront.Distribution {
  name = forwarded
}
"#,
    )]);

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli.iter().any(|diagnostic| {
            diagnostic.contains("Type mismatch")
                && diagnostic.contains("expected String")
                && diagnostic.contains("got Int")
        }),
        "CLI must retain the forwarded binding's reference type check: {cli:#?}",
    );
    assert!(
        lsp.iter().any(|diagnostic| {
            diagnostic.message.contains("Type mismatch")
                && diagnostic.message.contains("expected String")
                && diagnostic.message.contains("got Int")
        }),
        "LSP must retain the same forwarded binding type check: {lsp:#?}",
    );
}

#[test]
fn enum_member_shadowed_by_resource_binding_reports_cli_lsp_parity_and_quick_fix() {
    let schemas = enum_shadow_schemas();
    let fixture = enum_shadow_fixture("  event_type = viewer_request\n");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");

    let cli_message = cli
        .iter()
        .find(|message| message.contains(ENUM_SHADOW_MESSAGE))
        .expect("CLI enum-shadow diagnostic");
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message.contains(ENUM_SHADOW_MESSAGE))
        .unwrap_or_else(|| panic!("LSP enum-shadow diagnostic; got {lsp:#?}"));
    assert!(
        cli_message.contains(&diagnostic.message),
        "CLI and LSP must render the same typed error; CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert!(
        cli_message.contains("main.crn") && cli_message.contains("line 3"),
        "the CLI diagnostic must identify the resource owner's authored file and line: {cli_message}",
    );

    let line = "  event_type = viewer_request";
    let start = line.find("viewer_request").expect("value start") as u32;
    assert_eq!(diagnostic.range.start.line, 2);
    assert_eq!(diagnostic.range.end.line, 2);
    assert_eq!(diagnostic.range.start.character, start);
    assert_eq!(
        diagnostic.range.end.character,
        start + "viewer_request".len() as u32
    );

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    let actions = carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic);
    let action = actions
        .iter()
        .find(|action| {
            action.title == "Replace with `awscc.cloudfront.Distribution.EventType.viewer_request`"
        })
        .unwrap_or_else(|| panic!("enum-shadow quick fix; got {actions:#?}"));
    let edit = action
        .edit
        .as_ref()
        .and_then(|edit| edit.changes.as_ref())
        .and_then(|changes| changes.get(&uri))
        .and_then(|edits| edits.first())
        .expect("quick-fix text edit");
    assert_eq!(edit.range, diagnostic.range);
    assert_eq!(
        edit.new_text,
        "awscc.cloudfront.Distribution.EventType.viewer_request"
    );
}

#[test]
fn namespace_less_enum_shadow_keeps_the_mismatch_without_a_noop_quick_fix() {
    let schemas = bare_enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let fast = 42
test.r.mode_holder {
  mode = fast
}
"#,
    )]);

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let mismatch = "Type mismatch: expected Mode, got Int";
    assert!(
        cli.iter().any(|message| message.contains(mismatch)),
        "CLI must keep the ordinary mismatch when no distinct enum spelling exists: {cli:#?}",
    );
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == mismatch)
        .unwrap_or_else(|| panic!("LSP must keep the ordinary mismatch: {lsp:#?}"));
    assert!(
        cli.iter().all(|message| !message.contains("write `fast`"))
            && lsp
                .iter()
                .all(|diagnostic| !diagnostic.message.contains("write `fast`")),
        "a namespace-less enum must not suggest replacing a token with itself; CLI={cli:#?}; LSP={lsp:#?}",
    );

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    assert!(
        carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic).is_empty(),
        "the ordinary mismatch must not carry an enum quick fix",
    );
}

#[test]
fn pending_nested_resource_handle_reports_the_authored_use_instead_of_internal_jargon() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("nested resource-handle fixture");
    let module = fixture.path().join("module");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let source = awscc.test.Source { name = "source" }
let imported = use { source = "./module" }
imported { sources = [source] }
"#,
    )
    .expect("write caller fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments { sources: list(awscc.test.Source) }\n",
    )
    .expect("write module signature");

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let expected_message = "'source' is a resource, not a value; reference one of its attributes, e.g. 'source.<attribute>'";
    assert!(
        cli.iter().any(|message| {
            message.contains("'source' is a resource, not a value")
                && message.contains("main.crn")
                && message.contains("line 3")
        }),
        "the unrepresentable nested handle must point to the authored token: {cli:#?}",
    );
    assert_eq!(
        cli_located_message_multiset(&cli),
        BTreeMap::from([(expected_message.to_string(), 1)]),
        "the CLI must emit the authored pending-use finding exactly once: {cli:#?}",
    );
    assert_eq!(
        lsp_message_multiset(&lsp),
        cli_located_message_multiset(&cli),
        "CLI and LSP pending-use diagnostics must have the same messages and multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert!(
        cli.iter()
            .all(|message| !message.contains("unconsumed pending value slot")),
        "internal pending-slot terminology must not reach users: {cli:#?}",
    );
    assert!(
        lsp.iter()
            .all(|diagnostic| !diagnostic.message.contains("unconsumed pending value slot")),
        "internal pending-slot terminology must not reach LSP users: {lsp:#?}",
    );
}

#[test]
fn issue_2978_canonical_api_spelling_has_cli_lsp_parity_and_identity_derived_fix() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[
        (
            "bindings.crn",
            "let VPC = awscc.test.Source { name = \"shadow\" }\n",
        ),
        (
            "main.crn",
            r#"awscc.ec2.FlowLog {
  name = "flow-log"
  resource_type = VPC
}
"#,
        ),
    ]);
    let expected = "`VPC` was interpreted as the `let` binding; if you meant the enum `ResourceType` value, write `awscc.ec2.FlowLog.ResourceType.vpc`";
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert!(
        cli.iter().any(|message| message.contains(expected)),
        "CLI must recognize the canonical API spelling as a member: {cli:#?}",
    );
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == expected)
        .unwrap_or_else(|| panic!("LSP canonical-spelling diagnostic; got {lsp:#?}"));

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    let actions = carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic);
    let edit = actions
        .iter()
        .find(|action| action.title == "Replace with `awscc.ec2.FlowLog.ResourceType.vpc`")
        .and_then(|action| action.edit.as_ref())
        .and_then(|edit| edit.changes.as_ref())
        .and_then(|changes| changes.get(&uri))
        .and_then(|edits| edits.first())
        .unwrap_or_else(|| panic!("identity-derived canonical quick fix; got {actions:#?}"));
    assert_eq!(edit.range, diagnostic.range);
    assert_eq!(edit.new_text, "awscc.ec2.FlowLog.ResourceType.vpc");
}

#[test]
fn lsp_enum_shadow_ranges_use_exact_parser_spans_for_repeated_and_unicode_prefixed_tokens() {
    let schemas = enum_shadow_schemas();
    let source = r#"let viewer_request = awscc.test.Source { name = "shadow" }
awscc.cloudfront.Distribution {
  name = "distribution"
  event_types = [viewer_request, viewer_request]
  event_type_map = { "日本語" = viewer_request }
}
"#;
    let fixture = write_fixture(&[("main.crn", source)]);
    let diagnostics = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn")
        .into_iter()
        .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
        .collect::<Vec<_>>();
    assert_eq!(diagnostics.len(), 3, "one finding per authored token");

    let list_line = source.lines().nth(3).unwrap();
    let list_starts = list_line
        .match_indices("viewer_request")
        .map(|(byte, _)| list_line[..byte].chars().count() as u32)
        .collect::<Vec<_>>();
    assert_eq!(list_starts.len(), 2);
    let unicode_line = source.lines().nth(4).unwrap();
    let unicode_byte = unicode_line.find("viewer_request").unwrap();
    let unicode_start = unicode_line[..unicode_byte].chars().count() as u32;

    let ranges = diagnostics
        .iter()
        .map(|diagnostic| diagnostic.range)
        .collect::<Vec<_>>();
    for start in list_starts {
        assert!(ranges.iter().any(|range| {
            range.start.line == 3
                && range.start.character == start
                && range.end.character == start + "viewer_request".len() as u32
        }));
    }
    assert!(ranges.iter().any(|range| {
        range.start.line == 4
            && range.start.character == unicode_start
            && range.end.character == unicode_start + "viewer_request".len() as u32
    }));
}

#[test]
fn for_expansion_deduplicates_enum_shadow_diagnostics_by_authored_token() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"let viewer_request = awscc.test.Source { name = "shadow" }
for name in ["first", "second"] {
  awscc.cloudfront.Distribution {
    name = name
    event_type = viewer_request
  }
}
"#,
    )]);

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    assert_eq!(
        cli.iter()
            .filter(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE))
            .count(),
        1,
        "expanded owners must not duplicate one authored CLI finding: {cli:#?}",
    );
    assert_eq!(
        lsp.iter()
            .filter(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
            .count(),
        1,
        "expanded owners must not duplicate one authored LSP finding: {lsp:#?}",
    );
}

#[test]
fn enum_typed_module_argument_reports_resource_binding_shadow() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module enum-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        "let viewer_request = awscc.test.Source { name = \"shadow\" }\n",
    )
    .expect("write binding fixture");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }
inner {
  event_type = viewer_request
}
"#,
    )
    .expect("write module call fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_type: awscc.cloudfront.Distribution.EventType\n}\n",
    )
    .expect("write module signature");

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.contains(ENUM_SHADOW_MESSAGE)),
        "module argument must use the provider-owned enum declaration: {diagnostics:#?}",
    );
    let diagnostic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message == ENUM_SHADOW_MESSAGE)
        .unwrap_or_else(|| panic!("module LSP enum-shadow diagnostic; got {lsp:#?}"));
    let line = "  event_type = viewer_request";
    let start = line.find("viewer_request").expect("module value start") as u32;
    assert_eq!(diagnostic.range.start.line, 2);
    assert_eq!(diagnostic.range.end.line, 2);
    assert_eq!(diagnostic.range.start.character, start);
    assert_eq!(
        diagnostic.range.end.character,
        start + "viewer_request".len() as u32,
    );

    let uri = tower_lsp::lsp_types::Url::from_file_path(fixture.path().join("main.crn"))
        .expect("fixture URI");
    let actions = carina_lsp::code_action::code_actions_for_diagnostic(&uri, diagnostic);
    let edit = actions
        .iter()
        .find(|action| {
            action.title == "Replace with `awscc.cloudfront.Distribution.EventType.viewer_request`"
        })
        .and_then(|action| action.edit.as_ref())
        .and_then(|edit| edit.changes.as_ref())
        .and_then(|changes| changes.get(&uri))
        .and_then(|edits| edits.first())
        .unwrap_or_else(|| panic!("module enum-shadow quick fix; got {actions:#?}"));
    assert_eq!(edit.range, diagnostic.range);
    assert_eq!(
        edit.new_text,
        "awscc.cloudfront.Distribution.EventType.viewer_request",
    );
}

#[test]
fn nested_enum_typed_module_argument_descends_the_recorded_binding_path() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("nested module enum-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let viewer_request = awscc.test.Source { name = "shadow" }
let inner = use { source = "./modules/inner" }
inner {
  event_types = [viewer_request]
}
"#,
    )
    .expect("write nested caller fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_types: list(awscc.cloudfront.Distribution.EventType)\n}\n",
    )
    .expect("write nested module signature");

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let expected = BTreeMap::from([(ENUM_SHADOW_MESSAGE.to_string(), 1)]);
    assert!(
        matches!(cli.as_slice(), [message]
            if message.contains(ENUM_SHADOW_MESSAGE)
                && message.contains("main.crn")
                && message.contains("line 4")),
        "CLI must emit only the located shared binding-use finding: {cli:#?}",
    );
    assert_eq!(
        lsp_message_multiset(&lsp),
        expected,
        "LSP must suppress missing-argument and unused-binding secondaries: {lsp:#?}",
    );
    assert_eq!(
        cli_located_message_multiset(&cli),
        lsp_message_multiset(&lsp),
        "CLI and LSP must preserve the same diagnostic multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
    );
}

#[test]
fn successful_nested_binding_does_not_hide_an_invalid_module_argument_sibling() {
    let fixture = tempfile::tempdir().expect("nested module sibling fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let valid_name = "ok"
let inner = use { source = "./modules/inner" }
inner {
  names = [valid_name, 42]
}
"#,
    )
    .expect("write nested caller fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  names: list(String)\n}\n",
    )
    .expect("write nested module signature");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    assert!(
        cli.iter().any(|diagnostic| {
            diagnostic.contains("names")
                && diagnostic.contains("expected String")
                && diagnostic.to_ascii_lowercase().contains("got int")
        }),
        "CLI must retain ordinary validation for the invalid sibling: {cli:#?}",
    );
    assert!(
        lsp.iter().any(|diagnostic| {
            diagnostic.message.contains("expected String")
                && diagnostic.message.to_ascii_lowercase().contains("got int")
        }),
        "LSP must not skip the whole argument after the binding use succeeds: {lsp:#?}",
    );
}

#[test]
fn nested_value_binding_shadow_replaces_the_legacy_module_argument_error() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("nested value-shadow fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let viewer_request = 42
let inner = use { source = "./modules/inner" }
inner {
  event_types = [viewer_request]
}
"#,
    )
    .expect("write nested value-shadow fixture");
    std::fs::write(
        module.join("arguments.crn"),
        "arguments {\n  event_types: list(awscc.cloudfront.Distribution.EventType)\n}\n",
    )
    .expect("write nested module signature");

    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let expected = BTreeMap::from([(ENUM_SHADOW_MESSAGE.to_string(), 1)]);
    assert!(
        matches!(cli.as_slice(), [message]
            if message.contains(ENUM_SHADOW_MESSAGE)
                && message.contains("main.crn")
                && message.contains("line 4")),
        "the CLI authority must replace its legacy argument diagnostic with one located finding: {cli:#?}",
    );
    assert_eq!(
        lsp_message_multiset(&lsp),
        expected,
        "the LSP must expose exactly the same diagnostic set: {lsp:#?}",
    );
    assert_eq!(
        cli_located_message_multiset(&cli),
        lsp_message_multiset(&lsp),
        "CLI and LSP must preserve the same diagnostic multiplicities; CLI={cli:#?}; LSP={lsp:#?}",
    );
}

#[test]
fn module_value_binding_type_mismatch_has_exact_cli_lsp_diagnostic_parity() {
    let fixture = tempfile::tempdir().expect("module value mismatch fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let count = "not-an-int"
let inner = use { source = "./modules/inner" }
inner { count = count }
"#,
    )
    .expect("write caller fixture");
    std::fs::write(module.join("main.crn"), "arguments { count: Int }\n")
        .expect("write module signature");

    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &engine_with_schemas(SchemaRegistry::new()),
        &fixture,
        "main.crn",
    );
    assert!(
        matches!((cli.as_slice(), lsp.as_slice()), ([cli_message], [lsp_diagnostic])
            if cli_message.contains(&lsp_diagnostic.message)
                && cli_message.contains("main.crn")
                && cli_message.contains("line 3")),
        "one authored mismatch must yield the same message with a CLI source location; CLI={cli:#?}; LSP={lsp:#?}",
    );
    assert_eq!(
        lsp[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        "a binding-use finding that fails CLI validation must be an LSP error: {lsp:#?}",
    );
}

#[test]
fn enum_value_binding_keeps_lexical_precedence_and_preserves_the_bound_member() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[
        (
            "bindings.crn",
            "let viewer_request = awscc.cloudfront.Distribution.EventType.origin_request\n",
        ),
        (
            "main.crn",
            r#"awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = viewer_request
}
"#,
        ),
    ]);

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics.is_empty(),
        "an assignable enum-valued binding must win over shorthand: {diagnostics:#?}",
    );
    assert!(
        lsp.iter().all(|diagnostic| {
            !diagnostic.message.contains(ENUM_SHADOW_MESSAGE)
                && !diagnostic.message.contains("event_type")
        }),
        "LSP must accept the same enum-valued binding: {lsp:#?}",
    );

    let parsed = cli_validated_file(factories_for(&schemas), &fixture);
    let distribution = parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "cloudfront.Distribution")
        .expect("validated distribution");
    assert!(
        matches!(
            distribution.get_attr("event_type"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "OriginRequest"
        ),
        "the lexical binding must preserve the different `origin_request` member in the CLI tree: {:?}",
        distribution.get_attr("event_type"),
    );
}

#[test]
fn enum_typed_module_argument_preserves_the_bound_member_in_the_expanded_resource() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module enum-precedence fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("bindings.crn"),
        "let viewer_request = awscc.cloudfront.Distribution.EventType.origin_request\n",
    )
    .expect("write enum binding");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let inner = use { source = "./modules/inner" }
inner {
  event_type = viewer_request
}
"#,
    )
    .expect("write module call");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  event_type: awscc.cloudfront.Distribution.EventType
}

awscc.cloudfront.Distribution {
  name = "expanded"
  event_type = event_type
}
"#,
    )
    .expect("write module body");

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    assert!(
        lsp.iter().all(|diagnostic| {
            !diagnostic.message.contains(ENUM_SHADOW_MESSAGE)
                && !diagnostic.message.contains("viewer_request")
        }),
        "LSP must accept the enum-valued module argument binding: {lsp:#?}",
    );
    let parsed = cli_validated_file(factories_for(&schemas), &fixture);
    let expanded = parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "cloudfront.Distribution")
        .expect("expanded module distribution");
    assert!(
        matches!(
            expanded.get_attr("event_type"),
            Some(Value::Concrete(ConcreteValue::CanonicalEnum(value)))
                if value.api_value() == "OriginRequest"
        ),
        "the module argument must preserve the different `origin_request` member after CLI expansion: {:?}",
        expanded.get_attr("event_type"),
    );
}

#[test]
fn enum_typed_argument_binding_wins_and_string_argument_reports_shadow() {
    let schemas = enum_shadow_schemas();
    let matching = write_fixture(&[(
        "main.crn",
        r#"arguments {
  viewer_request: awscc.cloudfront.Distribution.EventType
}

awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = viewer_request
}
"#,
    )]);
    let bootstrap = carina_core::config_loader::load_configuration_with_config(
        &carina_core::paths::ResolvedPath::new(matching.path()).unwrap(),
        &carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
        &SchemaRegistry::new(),
    )
    .expect("schema-independent bootstrap parse");
    assert!(matches!(
        bootstrap
            .parsed
            .arguments
            .first()
            .map(|argument| &argument.static_type),
        Some(carina_core::parser::TypeExpr::DottedUnresolved(_))
    ));
    let matching_lsp =
        lsp_diagnostics(&engine_with_schemas(schemas.clone()), &matching, "main.crn");
    let matching_cli = cli_diagnostics(factories_for(&schemas), &matching);
    assert!(
        matching_cli.is_empty(),
        "an argument declared as the expected enum must keep lexical precedence: {matching_cli:#?}",
    );
    assert!(
        matching_lsp
            .iter()
            .all(|diagnostic| !diagnostic.message.contains("viewer_request")),
        "LSP must accept the same enum-typed argument: {matching_lsp:#?}",
    );

    let mismatched = write_fixture(&[(
        "main.crn",
        r#"arguments {
  viewer_request: String
}

awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = viewer_request
}
"#,
    )]);
    let expected = "`viewer_request` was interpreted as the `argument` binding; if you meant the enum `EventType` value, write `awscc.cloudfront.Distribution.EventType.viewer_request`";
    let mismatched_lsp = lsp_diagnostics(
        &engine_with_schemas(schemas.clone()),
        &mismatched,
        "main.crn",
    );
    let mismatched_cli = cli_diagnostics(factories_for(&schemas), &mismatched);
    assert!(
        mismatched_cli
            .iter()
            .any(|diagnostic| diagnostic.contains(expected)),
        "a String argument that shadows the enum member must use the structured error: {mismatched_cli:#?}",
    );
    assert!(
        mismatched_lsp
            .iter()
            .any(|diagnostic| diagnostic.message == expected),
        "LSP must render the same argument-shadow error: {mismatched_lsp:#?}",
    );
}

#[test]
fn fully_qualified_shape_preserves_a_bound_provider_head() {
    let schemas = enum_shadow_schemas();
    let fixture = write_fixture(&[
        (
            "bindings.crn",
            "let awscc = awscc.test.Source { name = \"shadow\" }\n",
        ),
        (
            "main.crn",
            r#"awscc.cloudfront.Distribution {
  name = "distribution"
  event_type = awscc.cloudfront.Distribution.EventType.viewer_request
}
"#,
        ),
    ]);

    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert_eq!(
        diagnostics,
        vec![
            "awscc.cloudfront.Distribution.<pending>: unknown attribute 'cloudfront' on 'awscc' in reference awscc.cloudfront"
                .to_string(),
        ],
        "the resource binding must retain precedence over enum-shaped spelling",
    );
}

#[test]
fn caller_let_forwarded_through_module_parameter_keeps_the_expanded_value_and_does_not_panic() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module forwarding fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create inner module");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let my_let = "caller-value"
let param = 42
let inner = use { source = "./modules/inner" }

awscc.test.Source {
  name = "root"
  count = param
}
inner {
  param = my_let
}
"#,
    )
    .expect("write caller fixture");
    std::fs::write(
        module.join("main.crn"),
        r#"arguments {
  param: String
}

awscc.cloudfront.Distribution {
  name = param
}
"#,
    )
    .expect("write module fixture");

    let config =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let loaded = carina_core::config_loader::load_configuration_with_config(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        &config,
        &schemas,
    )
    .expect("load caller configuration");
    let parsed =
        carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
            carina_core::module_resolver::resolve_modules_with_providerless_config(
                loaded.parsed,
                &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
                session,
                &schemas,
            )
            .expect("expand module call without panicking")
        });

    let expanded = parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "cloudfront.Distribution")
        .expect("expanded module resource");
    assert_eq!(
        expanded.get_attr("name"),
        Some(&Value::Concrete(ConcreteValue::String(
            "caller-value".into()
        ))),
        "module substitution must leave the ordinary runtime value clean",
    );

    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics.is_empty(),
        "the module parameter must not resolve against the root's same-named Int binding: {diagnostics:#?}",
    );
}

#[test]
fn module_internal_bare_use_ignores_a_same_named_root_let() {
    let schemas = enum_shadow_schemas();
    let fixture = tempfile::tempdir().expect("module lexical-scope fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).unwrap();
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let shared_name = 42
let inner = use { source = "./modules/inner" }
awscc.test.Source {
  name = "root"
  count = shared_name
}
inner {}
"#,
    )
    .unwrap();
    std::fs::write(
        module.join("main.crn"),
        r#"let shared_name = "module-value"
attributes { name = shared_name }
awscc.cloudfront.Distribution { name = shared_name }
"#,
    )
    .unwrap();

    let diagnostics = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        diagnostics.is_empty(),
        "module-internal provenance must resolve only in the module scope: {diagnostics:#?}",
    );
}

#[test]
fn enum_bare_typequalified_fully_qualified_all_pass_parity() {
    let schemas = enum_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.mode_holder {
    name = "a"
    mode = fast
}
test.r.mode_holder {
    name = "b"
    mode = Mode.slow
}
test.r.mode_holder {
    name = "c"
    mode = test.r.Mode.fast
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);

    assert_eq!(
        lsp_messages_count(&lsp_diags, "Mode"),
        cli_messages_count(&cli_diags, "Mode"),
        "LSP and CLI Mode-mention counts must agree.\nLSP: {:?}\nCLI: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
        cli_diags,
    );
    assert_eq!(
        lsp_messages_count(&lsp_diags, "Mode"),
        0,
        "expected no Mode diagnostics from LSP, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn enum_invalid_value_reports_diagnostic_parity() {
    let schemas = enum_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.mode_holder {
    name = "a"
    mode = "bogus"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);

    assert!(
        lsp_messages_contain(&lsp_diags, "bogus"),
        "LSP must surface bogus value, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "bogus"),
        "CLI must surface bogus value, got {:?}",
        cli_diags,
    );
}

#[test]
fn validate_with_factories_preserves_enum_canonicalization_detail_once() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let holder = missing.r.mode_holder {
    mode = test.r.Mode.bogus
}
"#,
    )]);

    let diagnostics = cli_diagnostics(Vec::new(), &fixture);
    let rendered = diagnostics.join("\n");
    let leaf = "unresolved enum identifier `test.r.Mode.bogus` survived canonicalization";
    assert!(rendered.contains(leaf), "missing enum detail: {rendered}");
    assert_eq!(
        rendered.matches(leaf).count(),
        1,
        "enum detail must appear exactly once: {rendered}"
    );
}

// ============================================================================
// Scenario 2: Custom type with `to_dsl` normalization (Region-like)
// ============================================================================

fn region_schemas() -> SchemaRegistry {
    fn validate_region(v: &Value) -> Result<(), String> {
        const VALID: &[&str] = &["ap-northeast-1", "us-west-2"];
        if let Value::Concrete(ConcreteValue::String(s)) = v {
            let normalized = fixture_enum_segment_as_aws_value(s.expose_plaintext());
            if VALID.contains(&normalized.as_str()) {
                return Ok(());
            }
            return Err(format!("invalid Region '{}'", s.expose_plaintext()));
        }
        Err("expected string".to_string())
    }
    let region_custom = AttributeType::enum_(
        carina_core::schema::enum_identity("Region", Some("test")),
        None,
        vec![],
        Some(legacy_validator(move |value| {
            validate_region(value).map_err(Into::into)
        })),
        Some(carina_core::schema::DslTransform::HyphenToUnderscore),
    );

    single_schema_map(
        ResourceSchema::new("r.region_holder")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("region", region_custom).build()),
    )
}

#[test]
fn region_accepts_bare_and_typequalified_forms_parity() {
    let schemas = region_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let a = test.r.region_holder {
    name = "a"
    region = ap_northeast_1
}
let b = test.r.region_holder {
    name = "b"
    region = Region.us_west_2
}
let c = test.r.region_holder {
    name = "c"
    region = test.Region.ap_northeast_1
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_region =
        lsp_messages_count(&lsp_diags, "Region") + lsp_messages_count(&lsp_diags, "region");
    let cli_region =
        cli_messages_count(&cli_diags, "Region") + cli_messages_count(&cli_diags, "region");
    assert_eq!(
        lsp_region,
        cli_region,
        "LSP / CLI Region-mention parity broken.\nLSP: {:?}\nCLI: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
        cli_diags,
    );
    assert_eq!(lsp_region, 0, "expected zero Region diagnostics");
}

#[test]
fn region_accepts_aws_string_form_parity() {
    let schemas = region_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let a = test.r.region_holder {
    name = "a"
    region = "ap-northeast-1"
}
let b = test.r.region_holder {
    name = "b"
    region = "us-west-2"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_region =
        lsp_messages_count(&lsp_diags, "Region") + lsp_messages_count(&lsp_diags, "region");
    let cli_region =
        cli_messages_count(&cli_diags, "Region") + cli_messages_count(&cli_diags, "region");
    assert_eq!(
        lsp_region,
        cli_region,
        "LSP / CLI Region-string-form parity broken.\nLSP: {:?}\nCLI: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
        cli_diags,
    );
    assert_eq!(lsp_region, 0, "expected zero Region diagnostics");
}

#[test]
fn region_rejects_unknown_value_parity() {
    let schemas = region_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.region_holder {
    name = "a"
    region = mars_1
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        lsp_messages_contain(&lsp_diags, "Region") || lsp_messages_contain(&lsp_diags, "mars"),
        "LSP must reject mars_1, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "Region") || cli_messages_contain(&cli_diags, "mars"),
        "CLI must reject mars_1, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario 3: nested-Struct field type mismatch
// ============================================================================

fn nested_struct_schemas() -> SchemaRegistry {
    let inner = AttributeType::struct_(
        "Inner".to_string(),
        vec![StructField::optional("leaf", AttributeType::int()).build()],
    );
    let outer = AttributeType::struct_(
        "Outer".to_string(),
        vec![
            StructField::optional("inner", inner).build(),
            StructField::optional("label", AttributeType::string()).build(),
        ],
    );

    single_schema_map(
        ResourceSchema::new("r.nested")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("outer", outer).build()),
    )
}

#[test]
fn nested_struct_int_field_with_string_value_diagnoses_parity() {
    let schemas = nested_struct_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.nested {
    name = "a"
    outer = {
        label = "x"
        inner = { leaf = "not-an-int" }
    }
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let needle_in = |has: &dyn Fn(&str) -> bool| has("leaf") || has("Int") || has("not-an-int");
    let lsp_hit = needle_in(&|s| lsp_messages_contain(&lsp_diags, s));
    let cli_hit = needle_in(&|s| cli_messages_contain(&cli_diags, s));
    assert!(lsp_hit, "LSP must diagnose nested int mismatch");
    assert!(
        cli_hit,
        "CLI must diagnose nested int mismatch, got {:?}",
        cli_diags
    );
}

// ============================================================================
// Scenario 4: Union with multiple member candidates
// ============================================================================

fn union_schemas() -> SchemaRegistry {
    let union = AttributeType::union(vec![AttributeType::int(), AttributeType::string()]);
    single_schema_map(
        ResourceSchema::new("r.union")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("value", union).build()),
    )
}

#[test]
fn union_accepts_either_member_parity() {
    let schemas = union_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.union {
    name = "a"
    value = 42
}
test.r.union {
    name = "b"
    value = "hello"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_mismatch = lsp_messages_count(&lsp_diags, "Type mismatch");
    let cli_mismatch = cli_messages_count(&cli_diags, "Type mismatch");
    assert_eq!(
        lsp_mismatch,
        0,
        "LSP: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(cli_mismatch, 0, "CLI: {:?}", cli_diags);
}

#[test]
fn union_rejects_non_member_type_parity() {
    let schemas = union_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.union {
    name = "a"
    value = true
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let any = |c: &dyn Fn(&str) -> bool| c("value") || c("Bool") || c("Union");
    assert!(
        any(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must reject Bool"
    );
    assert!(
        any(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must reject Bool, got {:?}",
        cli_diags
    );
}

#[test]
fn tied_scalar_union_reports_every_alternative_once_with_cli_lsp_parity() {
    let hosted_zone_id = AttributeType::union(vec![
        AttributeType::enum_(
            carina_core::schema::enum_identity(
                "CloudFrontHostedZoneId",
                Some("test.route53.Record.AliasTarget"),
            ),
            Some(vec!["global".to_string()]),
            vec![],
            None,
            None,
        ),
        AttributeType::refined_string(
            Some(carina_core::schema::TypeIdentity::new(
                Some("test"),
                ["route53", "Record", "AliasTarget"],
                "Route53HostedZoneId",
            )),
            Some(carina_core::schema::PatternSpec::new(
                "^Z[A-Z0-9]+$".to_string(),
                carina_core::schema::PatternMatch::Full,
            )),
            None,
            None,
        ),
    ]);
    let schema = ResourceSchema::new("route53.Record")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("hosted_zone_id", hosted_zone_id).build());
    let attributes: HashMap<String, Value> = HashMap::from([
        (
            "name".to_string(),
            Value::Concrete(ConcreteValue::String("record".into())),
        ),
        (
            "hosted_zone_id".to_string(),
            Value::Concrete(ConcreteValue::String("z14grhdcwa56qt".into())),
        ),
    ]);
    let validated_schema = schema
        .clone()
        .validate()
        .expect("test schema must be valid");
    let mut core_registry = SchemaRegistry::new();
    core_registry.insert("test", validated_schema);
    let core_factories = Arc::new(factories_for(&core_registry));
    let core_resolver = carina_core::provider::provider_custom_types_from_factories(core_factories);
    let classified_attributes = attributes
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                value
                    .knowledge(&carina_core::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification())
                    .unwrap(),
            )
        })
        .collect();
    let core_errors = carina_core::test_utils::admit_resource_attributes(
        core_registry
            .get(
                "test",
                "route53.Record",
                carina_core::schema::SchemaKind::Resource,
            )
            .expect("test schema must be owner-bound"),
        &classified_attributes,
        &|name| name == "hosted_zone_id",
        &core_resolver,
    )
    .expect_err("both union alternatives must reject the value");
    assert_eq!(core_errors.len(), 1, "core errors: {core_errors:#?}");
    let expected = core_errors[0].to_string();
    assert!(
        expected.contains("CloudFrontHostedZoneId.global") && expected.contains("/^Z[A-Z0-9]+$/"),
        "core must retain both tied failures: {expected}"
    );

    let schemas = single_schema_map(schema);
    let fixture = write_fixture(&[
        ("providers.crn", "provider test {}\n"),
        (
            "main.crn",
            r#"test.route53.Record {
  name = "record"
  hosted_zone_id = "z14grhdcwa56qt"
}
"#,
        ),
    ]);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_matching = lsp
        .iter()
        .filter(|diagnostic| diagnostic.message == expected)
        .collect::<Vec<_>>();
    let cli_matching = cli
        .iter()
        .filter(|diagnostic| diagnostic.contains(&expected))
        .collect::<Vec<_>>();

    assert_eq!(
        lsp_matching.len(),
        1,
        "LSP must report the shared message once: {lsp:#?}"
    );
    assert_eq!(
        cli_matching.len(),
        1,
        "CLI validate must report the shared message once: {cli:#?}"
    );
}

// ============================================================================
// Scenario 5: ResourceRef across sibling files
// ============================================================================

fn resource_ref_schemas() -> SchemaRegistry {
    let producer = ResourceSchema::new("r.producer")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::provider_populated("id", AttributeType::string()).build());
    let consumer = ResourceSchema::new("r.consumer")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("target_id", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        producer.validate().expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        consumer.validate().expect("schema fixture must be valid"),
    );
    schemas
}

#[test]
fn resource_ref_across_sibling_files_resolves_parity() {
    let schemas = resource_ref_schemas();
    let fixture = write_fixture(&[
        (
            "main.crn",
            r#"
test.r.consumer {
    name = "c1"
    target_id = upstream.id
}
"#,
        ),
        (
            "exports.crn",
            r#"
let upstream = test.r.producer {
    name = "p1"
}
"#,
        ),
    ]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp_undef = lsp_diags
        .iter()
        .filter(|d| d.message.contains("upstream") && d.message.contains("Undefined"))
        .count();
    let cli_undef = cli_diags
        .iter()
        .filter(|m| m.contains("upstream") && m.contains("Undefined"))
        .count();
    assert_eq!(
        lsp_undef,
        0,
        "LSP must resolve upstream from sibling file: {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
    assert_eq!(
        cli_undef, 0,
        "CLI must resolve upstream from sibling file: {:?}",
        cli_diags
    );
}

#[test]
fn read_only_assignment_in_multi_file_directory_fails_validate() {
    let schemas = single_schema_map(
        ResourceSchema::new("r.bucket")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("region", AttributeType::string()).build(),
            ),
    );
    let fixture = write_fixture(&[
        (
            "main.crn",
            r#"
test.r.bucket {
    name = "configured"
}
"#,
        ),
        (
            "sibling.crn",
            r#"
test.r.bucket {
    name = "sibling"
    region = "us-west-2"
}
"#,
        ),
    ]);

    let expected = "Attribute 'region' is populated by the provider and cannot be set";
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli_messages_contain(&cli_diags, expected),
        "carina validate must reject read-only assignment in a merged directory, got {cli_diags:?}"
    );

    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "sibling.crn");
    assert!(
        lsp_messages_contain(&lsp_diags, expected),
        "LSP must report the same read-only assignment, got {:?}",
        lsp_diags
            .iter()
            .map(|diagnostic| &diagnostic.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn nested_read_only_assignment_in_multi_file_directory_fails_validate() {
    let config = AttributeType::struct_(
        "Config".to_string(),
        vec![StructField::provider_populated("arn", AttributeType::string()).build()],
    );
    let schemas = single_schema_map(
        ResourceSchema::new("r.bucket")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("config", config).build()),
    );
    let fixture = write_fixture(&[
        (
            "main.crn",
            r#"
test.r.bucket {
    name = "configured"
}
"#,
        ),
        (
            "sibling.crn",
            r#"
test.r.bucket {
    name = "sibling"
    config = {
        arn = "provider-arn"
    }
}
"#,
        ),
    ]);

    let expected = "Attribute 'arn' is populated by the provider and cannot be set";
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    assert!(
        cli_messages_contain(&cli_diags, expected),
        "carina validate must reject nested read-only assignment in a merged directory, got {cli_diags:?}"
    );

    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "sibling.crn");
    let diagnostic = lsp_diags
        .iter()
        .find(|diagnostic| diagnostic.message.contains(expected))
        .unwrap_or_else(|| {
            panic!(
                "LSP must report the same nested read-only assignment, got {:?}",
                lsp_diags
                    .iter()
                    .map(|diagnostic| &diagnostic.message)
                    .collect::<Vec<_>>()
            )
        });
    assert_eq!(
        diagnostic.severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR),
        "LSP parity includes hard-error severity for nested writability"
    );
}

// ============================================================================
// Scenario 6: Unknown attribute with `suggestion`
// ============================================================================

#[test]
fn unknown_attribute_emits_suggestion_parity() {
    let schemas = single_schema_map(
        ResourceSchema::new("r.suggester")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("description", AttributeType::string()).build()),
    );
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.suggester {
    name = "a"
    descritpion = "x"
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let has_suggestion = |c: &dyn Fn(&str) -> bool| c("descritpion") && c("description");
    assert!(
        has_suggestion(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must suggest 'description', got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        has_suggestion(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must suggest 'description', got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario 3b: List<Struct> with nested-Struct field error (#2249)
// ============================================================================

fn list_struct_schemas() -> SchemaRegistry {
    let inner = AttributeType::struct_(
        "Inner".to_string(),
        vec![StructField::optional("leaf", AttributeType::int()).build()],
    );
    let outer = AttributeType::list(AttributeType::struct_(
        "Outer".to_string(),
        vec![
            StructField::optional("inner", inner).build(),
            StructField::optional("label", AttributeType::string()).build(),
        ],
    ));
    single_schema_map(
        ResourceSchema::new("r.list_nested")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("outer", outer)
                    .with_block_name("outer")
                    .build(),
            ),
    )
}

#[test]
fn list_struct_int_field_with_string_value_diagnoses_parity() {
    let schemas = list_struct_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.list_nested {
    name = "a"
    outer {
        label = "first"
        inner = { leaf = 30 }
    }
    outer {
        label = "second"
        inner = { leaf = "not-an-int" }
    }
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let any = |c: &dyn Fn(&str) -> bool| c("leaf") || c("Int") || c("not-an-int");
    assert!(
        any(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must diagnose inner.leaf int mismatch, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        any(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must diagnose inner.leaf int mismatch, got {:?}",
        cli_diags,
    );
}

fn list_struct_renamed_block_schemas() -> SchemaRegistry {
    let rule = AttributeType::list(AttributeType::struct_(
        "Rule".to_string(),
        vec![StructField::optional("days", AttributeType::int()).build()],
    ));
    single_schema_map(
        ResourceSchema::new("r.renamed_block")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("rules", rule)
                    .with_block_name("rule")
                    .build(),
            ),
    )
}

#[test]
fn list_struct_renamed_block_int_field_with_string_value_diagnoses_parity() {
    let schemas = list_struct_renamed_block_schemas();
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
test.r.renamed_block {
    name = "a"
    rule {
        days = 7
    }
    rule {
        days = "not-an-int"
    }
}
"#,
    )]);
    let lsp_diags = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_for(&schemas), &fixture);
    let any = |c: &dyn Fn(&str) -> bool| c("days") || c("Int") || c("not-an-int");
    assert!(
        any(&|s| lsp_messages_contain(&lsp_diags, s)),
        "LSP must diagnose rule.days int mismatch, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        any(&|s| cli_messages_contain(&cli_diags, s)),
        "CLI must diagnose rule.days int mismatch, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario: WASM-plugin Custom validator bridge (#2354)
//
// A schema produced via the WASM plugin path carries `validate: None`
// on every `AttributeType::Custom`, with the real validator behind the
// factory's `validate_custom_type`. The validation pipeline must reach
// through the loaded `ProviderContext` resolver so a bad `vpc_id`
// value is rejected.
// ============================================================================

struct WasmStyleProviderFactory {
    name: String,
    schemas: Vec<ValidatedResourceSchema>,
}

impl ProviderFactory for WasmStyleProviderFactory {
    fn name(&self) -> &str {
        &self.name
    }
    fn display_name(&self) -> &str {
        "WASM-style test provider"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        Default::default()
    }
    fn validate_config(
        &self,
        _attributes: &carina_core::provider::CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> carina_core::provider::ProviderResult<()> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        identity: &carina_core::schema::TypeIdentity,
        value: &carina_core::provider::CanonicalCustomTypeValue,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> carina_core::provider::ProviderResult<()> {
        // Mirror awscc's `prefixed` validator family: VpcId must start
        // with "vpc-".
        if identity.kind == "VpcId" && !value.as_str().starts_with("vpc-") {
            return Err(carina_core::provider::ProviderError::invalid_input(
                format!("Invalid vpc_id '{}': must start with 'vpc-'", value),
            ));
        }
        if identity.kind == "SubnetId" && !value.as_str().starts_with("subnet-") {
            return Err(carina_core::provider::ProviderError::invalid_input(
                format!("Invalid subnet_id '{}': must start with 'subnet-'", value),
            ));
        }
        if identity.kind == "MaxAggregationInterval" {
            let value = value.as_str().parse::<i64>().map_err(|error| {
                carina_core::provider::ProviderError::invalid_input(format!(
                    "invalid aggregation interval: {error}"
                ))
            })?;
            if !matches!(value, 60 | 600) {
                return Err(carina_core::provider::ProviderError::invalid_input(
                    format!("max_aggregation_interval must be 60 or 600, got {value}"),
                ));
            }
        }
        Ok(())
    }
    fn create_provider(
        &self,
        _config: &carina_core::provider::ProviderSession,
        _initialize_outcome: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<
        '_,
        carina_core::provider::ProviderResult<carina_core::provider::ProviderInitialization>,
    > {
        Box::pin(async {
            Ok(carina_core::provider::InitializeOutcome::unavailable()
                .with_provider(Box::new(NoopProvider)))
        })
    }
    fn create_normalizer(
        &self,
        _config: &carina_core::provider::ProviderSession,
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
        self.schemas.clone()
    }
}

fn wasm_style_vpc_schema() -> ResourceSchema {
    let vpc_id_type = AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        None,
        None,
        None,
    );
    ResourceSchema::new("ec2.security_group")
        .attribute(AttributeSchema::optional("group_description", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_type).build())
}

fn wasm_style_factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".to_string(),
        schemas: vec![
            wasm_style_vpc_schema()
                .validate()
                .expect("factory test schema must be valid"),
            wasm_style_flow_log_schema()
                .validate()
                .expect("numeric bridge factory schema must be valid"),
        ],
    }) as Box<dyn ProviderFactory>]
}

fn wasm_style_engine() -> DiagnosticEngine {
    let factories: Vec<Box<dyn ProviderFactory>> = wasm_style_factories();
    let provider_names = vec!["awsccmock".to_string()];
    let loaded_providers = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    DiagnosticEngine::new(Arc::new(loaded_providers), provider_names)
}

// carina#4276: local and provider checks must accept the same leaf / Union arm.
// Keep CLI and LSP assertions in separate tests so both failures are observable.
fn single_value_walk_cli_errors(
    factories: Vec<Box<dyn ProviderFactory>>,
    fixture: &TempDir,
) -> Vec<String> {
    let diagnostics = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories),
        Default::default(),
    );
    let errors: Vec<_> = diagnostics
        .errors
        .iter()
        .map(|error| carina_cli::error::format_error_with_source_chain(error))
        .collect();
    eprintln!(
        "CLI errors={errors:#?}\nwarnings={:#?}",
        diagnostics.warnings
    );
    assert!(
        diagnostics.warnings.is_empty(),
        "{:#?}",
        diagnostics.warnings
    );
    errors
}

fn single_value_walk_lsp_diagnostics(
    factories: Vec<Box<dyn ProviderFactory>>,
    fixture: &TempDir,
) -> Vec<tower_lsp::lsp_types::Diagnostic> {
    let provider_names = factories
        .iter()
        .map(|factory| factory.name().to_string())
        .collect();
    let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(loaded), provider_names);
    lsp_diagnostics(&engine, fixture, "main.crn")
}

fn single_value_walk_lsp_errors(
    factories: Vec<Box<dyn ProviderFactory>>,
    fixture: &TempDir,
) -> Vec<String> {
    single_value_walk_lsp_diagnostics(factories, fixture)
        .into_iter()
        .filter(|diagnostic| {
            diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
        })
        .map(|diagnostic| diagnostic.message)
        .collect()
}

// Nested portable findings retain main's warning severity; count those as
// validation findings while excluding the block-syntax authoring hint.
fn single_value_walk_lsp_value_findings(
    factories: Vec<Box<dyn ProviderFactory>>,
    fixture: &TempDir,
) -> Vec<String> {
    single_value_walk_lsp_diagnostics(factories, fixture)
        .into_iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.severity,
                Some(
                    tower_lsp::lsp_types::DiagnosticSeverity::ERROR
                        | tower_lsp::lsp_types::DiagnosticSeverity::WARNING
                )
            )
        })
        .map(|diagnostic| diagnostic.message)
        .collect()
}

fn single_value_walk_f03_fixture() -> (TempDir, SchemaRegistry) {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            include_str!("fixtures/single_value_walk/f03_portable_length/providers.crn"),
        ),
        (
            "main.crn",
            include_str!("fixtures/single_value_walk/f03_portable_length/main.crn"),
        ),
    ]);
    let params = AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(3))),
        None,
    );
    let schemas = single_schema_map(
        ResourceSchema::new("ec2.vpc")
            .attribute(AttributeSchema::optional("params", params).build()),
    );
    (fixture, schemas)
}

fn assert_single_value_walk_f03_errors(errors: &[String]) {
    assert_eq!(
        errors.len(),
        1,
        "F03 must emit exactly one local error: {errors:#?}"
    );
    assert!(
        errors[0].contains("Invalid value 'abcd' for 'params' (VpcId)")
            && errors[0].contains("length 4 is outside allowed range [, 3]"),
        "F03 must retain the local length diagnostic: {errors:#?}",
    );
    assert!(
        !errors[0].contains("rejected `VpcId`"),
        "F03 must skip provider declaration validation after local rejection: {errors:#?}",
    );
}

#[test]
fn single_value_walk_f03_cli_reports_only_local_length_error() {
    let (fixture, schemas) = single_value_walk_f03_fixture();
    let errors = single_value_walk_cli_errors(factories_for(&schemas), &fixture);
    assert_single_value_walk_f03_errors(&errors);
}

#[test]
fn single_value_walk_f03_lsp_reports_only_local_length_error() {
    let (fixture, schemas) = single_value_walk_f03_fixture();
    let errors = single_value_walk_lsp_errors(factories_for(&schemas), &fixture);
    assert_single_value_walk_f03_errors(&errors);
}

fn single_value_walk_m01_fixture() -> TempDir {
    write_fixture(&[
        (
            "providers.crn",
            include_str!("fixtures/single_value_walk/m01_union_provider/providers.crn"),
        ),
        (
            "main.crn",
            include_str!("fixtures/single_value_walk/m01_union_provider/main.crn"),
        ),
    ])
}

fn single_value_walk_m01_vpc_id_type() -> AttributeType {
    AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        None,
        None,
        None,
    )
}

fn single_value_walk_m01_factories(params: AttributeType) -> Vec<Box<dyn ProviderFactory>> {
    let schema = ResourceSchema::new("test.Sink")
        .attribute(AttributeSchema::optional("params", params).build())
        .validate()
        .expect("M01 schema must be valid");
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".to_string(),
        schemas: vec![schema],
    })]
}

fn round1_union_schema(nested: bool) -> ResourceSchema {
    let params = if nested {
        AttributeType::union(vec![AttributeType::ref_("Inner"), AttributeType::bool()])
    } else {
        AttributeType::union(vec![
            single_value_walk_m01_vpc_id_type(),
            AttributeType::refined_string(
                None,
                Some(carina_core::schema::PatternSpec::new(
                    "^x",
                    carina_core::schema::PatternMatch::Full,
                )),
                None,
                None,
            ),
        ])
    };
    ResourceSchema::new("test.Sink")
        .attribute(AttributeSchema::optional("params", params).build())
        .with_def(
            "Inner",
            AttributeType::union(vec![
                AttributeType::map(single_value_walk_m01_vpc_id_type()),
                AttributeType::struct_(
                    "Named",
                    vec![StructField::required("name", AttributeType::string()).build()],
                ),
            ]),
        )
}

fn round2_local_union_case(tie: bool) -> (AttributeType, &'static str, &'static str) {
    if tie {
        (
            AttributeType::union(vec![
                AttributeType::struct_(
                    "First",
                    vec![
                        StructField::required("v", single_value_walk_m01_vpc_id_type()).build(),
                        StructField::required("x", AttributeType::int()).build(),
                    ],
                ),
                AttributeType::struct_(
                    "Second",
                    vec![
                        StructField::required(
                            "v",
                            AttributeType::refined_string(
                                Some(carina_core::schema::TypeIdentity::bare("SubnetId")),
                                None,
                                None,
                                None,
                            ),
                        )
                        .build(),
                        StructField::required("y", AttributeType::int()).build(),
                    ],
                ),
            ]),
            "awsccmock.test.Sink {\n  params = { v = \"bad\" }\n}\n",
            "Expected one of these forms: ('v' and 'x') or ('v' and 'y')",
        )
    } else {
        (
            AttributeType::union(vec![
                AttributeType::struct_(
                    "Named",
                    vec![
                        StructField::optional("name", single_value_walk_m01_vpc_id_type()).build(),
                        StructField::required("size", AttributeType::int()).build(),
                    ],
                ),
                AttributeType::bool(),
            ]),
            "awsccmock.test.Sink {\n  params = { name = \"bad\" }\n}\n",
            "Required attribute 'size' is missing",
        )
    }
}

#[test]
fn issue_4276_round2_a_local_union_cli_keeps_one_arms_provider_findings() {
    for tie in [false, true] {
        let (ty, source, local) = round2_local_union_case(tie);
        let fixture = write_fixture(&[
            ("providers.crn", "provider awsccmock {}\n"),
            ("main.crn", source),
        ]);
        assert_eq!(
            single_value_walk_cli_errors(single_value_walk_m01_factories(ty), &fixture),
            vec![
                format!("awsccmock.test.Sink.<pending>: {local}"),
                "awsccmock.test.Sink.<pending>: awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'".into(),
            ],
            "tie={tie}",
        );
    }
}

#[test]
fn issue_4276_round2_a_local_union_lsp_keeps_one_arms_provider_findings() {
    for tie in [false, true] {
        let (ty, source, local) = round2_local_union_case(tie);
        let fixture = write_fixture(&[
            ("providers.crn", "provider awsccmock {}\n"),
            ("main.crn", source),
        ]);
        let mut expected = vec![local];
        if !tie {
            // MissingRequired retains both main's container warning and
            // resource-level error, independently of provider findings.
            expected.push(local);
        }
        expected.push("awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'");
        assert_eq!(
            single_value_walk_lsp_value_findings(single_value_walk_m01_factories(ty), &fixture),
            expected,
        );
    }
}

fn round1_union_factories(nested: bool) -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".into(),
        schemas: vec![round1_union_schema(nested).validate().unwrap()],
    })]
}

fn round2_provider_union_type(tie: bool, reverse: bool) -> AttributeType {
    let subnet = AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("SubnetId")),
        None,
        None,
        None,
    );
    let mut arms = vec![
        AttributeType::map(subnet),
        AttributeType::struct_(
            "Named",
            vec![
                StructField::required("a", single_value_walk_m01_vpc_id_type()).build(),
                StructField::required(
                    "b",
                    if tie {
                        AttributeType::string()
                    } else {
                        single_value_walk_m01_vpc_id_type()
                    },
                )
                .build(),
            ],
        ),
    ];
    if reverse {
        arms.reverse();
    }
    AttributeType::union(arms)
}

fn round2_provider_union_rejection(cli: bool) {
    let fixture = write_fixture(&[
        ("providers.crn", "provider awsccmock {}\n"),
        (
            "main.crn",
            "awsccmock.test.Sink {\n  params = { a = \"bad\", b = \"subnet-1\" }\n}\n",
        ),
    ]);
    for tie in [false, true] {
        for reverse in [false, true] {
            let factories =
                single_value_walk_m01_factories(round2_provider_union_type(tie, reverse));
            let (errors, prefix) = if cli {
                (
                    single_value_walk_cli_errors(factories, &fixture),
                    "awsccmock.test.Sink.<pending>: ",
                )
            } else {
                (
                    single_value_walk_lsp_value_findings(factories, &fixture),
                    "",
                )
            };
            let message = if tie && reverse {
                "awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'"
            } else {
                "awsccmock rejected `SubnetId`: Invalid subnet_id 'bad': must start with 'subnet-'"
            };
            assert_eq!(
                errors,
                vec![format!("{prefix}{message}")],
                "tie={tie}, reverse={reverse}"
            );
        }
    }
}

#[test]
fn issue_4276_round2_b_provider_only_union_cli_uses_fewest_errors() {
    round2_provider_union_rejection(true);
}

#[test]
fn issue_4276_round2_b_provider_only_union_lsp_uses_fewest_errors() {
    round2_provider_union_rejection(false);
}

fn round1_union_rejection(cli: bool, nested: bool) {
    let fixture = write_fixture(&[
        ("providers.crn", "provider awsccmock {}\n"),
        (
            "main.crn",
            if nested {
                "awsccmock.test.Sink {\n  params = { foo = \"bad\" }\n}\n"
            } else {
                "awsccmock.test.Sink {\n  params = \"bad\"\n}\n"
            },
        ),
    ]);
    let message = "awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'";
    let (errors, expected) = if cli {
        (
            single_value_walk_cli_errors(round1_union_factories(nested), &fixture),
            format!("awsccmock.test.Sink.<pending>: {message}"),
        )
    } else {
        let diagnostics =
            single_value_walk_lsp_diagnostics(round1_union_factories(nested), &fixture);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            diagnostics[0].severity,
            Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
        );
        assert_eq!(
            diagnostics[0].range,
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(1, 2),
                tower_lsp::lsp_types::Position::new(1, 8)
            )
        );
        (
            diagnostics.into_iter().map(|d| d.message).collect(),
            message.to_string(),
        )
    };
    assert_eq!(
        errors,
        vec![expected],
        "one provider rejection from the locally accepted arm"
    );
}

#[test]
fn issue_4276_round1_nested_union_cli_rejects() {
    round1_union_rejection(true, true);
}

#[test]
fn issue_4276_round1_nested_union_lsp_rejects() {
    round1_union_rejection(false, true);
}

#[test]
fn issue_4276_round1_union_tie_cli_rejects_once() {
    round1_union_rejection(true, false);
}

#[test]
fn issue_4276_round1_union_tie_lsp_rejects_once() {
    round1_union_rejection(false, false);
}

#[test]
fn issue_4276_round1_union_without_provider_constraint_accepts() {
    let fixture = single_value_walk_m01_fixture();
    for nested in [false, true] {
        if nested {
            std::fs::write(
                fixture.path().join("main.crn"),
                "awsccmock.test.Sink { params = { foo = \"bad\" } }\n",
            )
            .unwrap();
        }
        let factories = || {
            vec![Box::new(TestProviderFactory {
                name: "awsccmock".into(),
                schemas: vec![round1_union_schema(nested).validate().unwrap()],
            }) as Box<dyn ProviderFactory>]
        };
        assert!(single_value_walk_cli_errors(factories(), &fixture).is_empty());
        assert!(single_value_walk_lsp_value_findings(factories(), &fixture).is_empty());
    }
}

fn round1_short_factories() -> Vec<Box<dyn ProviderFactory>> {
    single_value_walk_m01_factories(AttributeType::refined_string(
        None,
        None,
        Some((None, Some(3))),
        None,
    ))
}

fn round2_string_list_defaults(cli: bool) {
    // The protocol default's StringList-to-List projection is pinned in core.
    let decoded = Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
        ConcreteValue::String("bad".into()),
    )]));
    for default in [
        Value::Concrete(ConcreteValue::StringList(vec!["bad".into()])),
        decoded,
    ] {
        for authored in [false, true] {
            let fixture = write_fixture(&[
                ("providers.crn", "provider awsccmock {}\n"),
                (
                    "main.crn",
                    if authored {
                        "awsccmock.test.Sink { params = [\"bad\"] }\n"
                    } else {
                        "awsccmock.test.Sink {}\n"
                    },
                ),
            ]);
            let factories: Vec<Box<dyn ProviderFactory>> =
                vec![Box::new(WasmStyleProviderFactory {
                    name: "awsccmock".into(),
                    schemas: vec![
                        ResourceSchema::new("test.Sink")
                            .attribute(
                                AttributeSchema::optional(
                                    "params",
                                    AttributeType::list(single_value_walk_m01_vpc_id_type()),
                                )
                                .with_default(default.clone())
                                .build(),
                            )
                            .validate()
                            .unwrap(),
                    ],
                })];
            let errors = if cli {
                single_value_walk_cli_errors(factories, &fixture)
            } else {
                single_value_walk_lsp_value_findings(factories, &fixture)
            };
            let expected = if authored {
                vec![format!(
                    "{}awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'",
                    if cli {
                        "awsccmock.test.Sink.<pending>: "
                    } else {
                        ""
                    }
                )]
            } else {
                Vec::new()
            };
            assert_eq!(errors, expected, "authored={authored}");
        }
    }
}

#[test]
fn issue_4276_round2_d_string_list_defaults_do_not_invoke_cli_provider_validation() {
    round2_string_list_defaults(true);
}

#[test]
fn issue_4276_round2_d_string_list_defaults_do_not_invoke_lsp_provider_validation() {
    round2_string_list_defaults(false);
}

#[test]
fn issue_4276_round1_module_if_reports_undecided_branch_constraints() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider awsccmock {}\n"),
        (
            "main.crn",
            "let child = use { source = './child' }\nchild { flag = true }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(
        fixture.path().join("child/arguments.crn"),
        "arguments { flag: Bool }\n",
    )
    .unwrap();
    std::fs::write(
        fixture.path().join("child/main.crn"),
        "awsccmock.test.Sink {\n  params = if flag { \"ok\" } else { \"too long\" }\n}\n",
    )
    .unwrap();
    assert!(single_value_walk_cli_errors(round1_short_factories(), &fixture).is_empty());
    let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(round1_short_factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(loaded), vec!["awsccmock".into()]);
    let diagnostics = lsp_diagnostics(&engine, &fixture, "child/main.crn");
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec!["Invalid value 'too long' for 'params': length 8 is outside allowed range [, 3]",]
    );
}

fn round2_module_if_fixture(flag: &str, first: &str) -> TempDir {
    let source = if flag == "gate.flag" {
        "let gate = awsccmock.test.Source {}\n"
    } else {
        ""
    };
    let fixture = write_fixture(&[
        ("providers.crn", "provider awsccmock {}\n"),
        (
            "main.crn",
            &format!(
                "{source}let child = use {{ source = './child' }}\nchild {{ flag = {flag} }}\n"
            ),
        ),
    ]);
    std::fs::create_dir(fixture.path().join("child")).unwrap();
    std::fs::write(
        fixture.path().join("child/arguments.crn"),
        "arguments { flag: Bool }\n",
    )
    .unwrap();
    std::fs::write(fixture.path().join("child/main.crn"), format!("awsccmock.test.Sink {{\n  params = if flag {{ \"{first}\" }} else {{ \"vpc-123\" }}\n}}\n")).unwrap();
    fixture
}

fn round2_module_if_factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".into(),
        schemas: vec![
            ResourceSchema::new("test.Sink")
                .attribute(
                    AttributeSchema::optional(
                        "params",
                        AttributeType::refined_string(
                            Some(carina_core::schema::TypeIdentity::bare("VpcId")),
                            None,
                            Some((None, Some(5))),
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
            ResourceSchema::new("test.Source")
                .attribute(
                    AttributeSchema::provider_populated("flag", AttributeType::bool()).build(),
                )
                .validate()
                .unwrap(),
        ],
    })]
}

#[test]
fn issue_4276_round2_c_identified_module_if_cli_checks_expanded_value_once() {
    for flag in ["true", "false"] {
        let fixture = round2_module_if_fixture(flag, "vpc-1");
        let expected = if flag == "true" {
            Vec::new()
        } else {
            vec![
                "awsccmock.test.Sink.<pending>: Invalid value 'vpc-123' for 'params' (VpcId): length 7 is outside allowed range [, 5]",
            ]
        };
        assert_eq!(
            single_value_walk_cli_errors(round2_module_if_factories(), &fixture),
            expected,
            "flag={flag}"
        );
    }
}

#[test]
fn issue_4276_round2_c_identified_module_if_lsp_reports_local_branch_error() {
    let fixture = round2_module_if_fixture("true", "vpc-1");
    let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(round2_module_if_factories()),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(loaded), vec!["awsccmock".into()]);
    let diagnostics = lsp_diagnostics(&engine, &fixture, "child/main.crn");
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Invalid value 'vpc-123' for 'params' (VpcId): length 7 is outside allowed range [, 5]",
        ]
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(1, 2), Position::new(1, 8))
    );
}

#[test]
fn issue_4276_round2_c_module_if_cli_rejects_runtime_condition_before_value_validation() {
    for both_local in [false, true] {
        let fixture =
            round2_module_if_fixture("gate.flag", if both_local { "bad-long" } else { "bad" });
        let expected = vec![
            "Module resolution error: module 'child' (child/main.crn) at line 2: if condition depends on a runtime value; condition must be statically known at parse time",
        ];
        assert_eq!(
            single_value_walk_cli_errors(round2_module_if_factories(), &fixture),
            expected,
            "both_local={both_local}"
        );
    }
}

#[test]
fn issue_4276_round1_module_if_expansion_reports_chosen_branch_once() {
    for (flag, expected) in [(true, 0), (false, 1)] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider awsccmock {}\n"),
            (
                "main.crn",
                &format!("let child = use {{ source = './child' }}\nchild {{ flag = {flag} }}\n"),
            ),
        ]);
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/arguments.crn"),
            "arguments { flag: Bool }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("child/main.crn"),
            "awsccmock.test.Sink {\n  params = if flag { \"ok\" } else { \"too long\" }\n}\n",
        )
        .unwrap();
        let cli = single_value_walk_cli_errors(round1_short_factories(), &fixture);
        assert_eq!(cli.len(), expected, "flag={flag}: {cli:#?}");
        if !flag {
            assert_eq!(
                cli[0],
                "awsccmock.test.Sink.<pending>: Invalid value 'too long' for 'params': length 8 is outside allowed range [, 3]"
            );
        }
    }
}

fn round1_group_map_type() -> AttributeType {
    use carina_core::schema::FieldGroup;
    AttributeType::map(AttributeType::struct_(
        "Entry",
        vec![
            StructField::optional(
                "a",
                AttributeType::struct_with_field_groups(
                    "Choice",
                    vec![
                        StructField::optional("first", AttributeType::string()).build(),
                        StructField::optional("second", AttributeType::string()).build(),
                    ],
                    vec![FieldGroup::exactly_one(["first", "second"]).unwrap()],
                ),
            )
            .build(),
            StructField::optional("b", AttributeType::int()).build(),
        ],
    ))
}

#[test]
fn issue_4276_round1_map_field_group_keeps_main_lsp_message() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider awsccmock {}\n"),
        (
            "main.crn",
            "awsccmock.test.Sink {\n  params = { entry = { a = {}, b = \"bad\" } }\n}\n",
        ),
    ]);
    let cli = single_value_walk_cli_errors(
        single_value_walk_m01_factories(round1_group_map_type()),
        &fixture,
    );
    assert_eq!(
        cli,
        vec![
            "awsccmock.test.Sink.<pending>: params.entry.a: exactly one of [first, second] must be set"
        ]
    );
    let diagnostics = single_value_walk_lsp_diagnostics(
        single_value_walk_m01_factories(round1_group_map_type()),
        &fixture,
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "params.entry.a: exactly one of [first, second] must be set",
            "Map value for key 'entry': params.a: exactly one of [first, second] must be set",
        ]
    );
    assert!(
        diagnostics
            .iter()
            .all(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
        "{diagnostics:#?}"
    );
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(1, 23), Position::new(1, 24))
    );
    assert_eq!(
        diagnostics[1].range,
        Range::new(Position::new(1, 2), Position::new(1, 8))
    );
}

#[test]
fn issue_4276_round1_provider_checks_struct_block_alias() {
    let params = AttributeType::struct_(
        "Settings",
        vec![
            StructField::optional(
                "entries",
                AttributeType::list(single_value_walk_m01_vpc_id_type()),
            )
            .with_block_name("entry")
            .build(),
        ],
    );
    let fixture = write_fixture(&[
        ("providers.crn", "provider awsccmock {}\n"),
        (
            "main.crn",
            "awsccmock.test.Sink {\n  params = { entry = [\"bad\"] }\n}\n",
        ),
    ]);
    let cli =
        single_value_walk_cli_errors(single_value_walk_m01_factories(params.clone()), &fixture);
    assert_eq!(
        cli,
        vec![
            "awsccmock.test.Sink.<pending>: awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'"
        ]
    );
    let diagnostics =
        single_value_walk_lsp_diagnostics(single_value_walk_m01_factories(params), &fixture);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'"
    );
    assert_eq!(
        diagnostics[0].severity,
        Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
    );
    assert_eq!(
        diagnostics[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 2),
            tower_lsp::lsp_types::Position::new(1, 8)
        )
    );
}

struct Round1ConfigFactory;

impl ProviderFactory for Round1ConfigFactory {
    fn name(&self) -> &str {
        "awsccmock"
    }
    fn display_name(&self) -> &str {
        "round 1 config provider"
    }
    fn provider_config_attribute_types(&self) -> carina_core::schema::ValidatedConfigSchema {
        carina_core::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
            "params".into(),
            AttributeType::list(AttributeType::refined_string(
                Some(carina_core::schema::TypeIdentity::bare("VpcId")),
                None,
                Some((None, Some(3))),
                None,
            )),
        )]))
        .unwrap()
    }
    fn validate_config(
        &self,
        _: &carina_core::provider::CanonicalProviderConfigAttributes,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        _: &carina_core::schema::TypeIdentity,
        _: &carina_core::provider::CanonicalCustomTypeValue,
        _session: &carina_core::provider::ProviderValidationInputs,
    ) -> ProviderResult<()> {
        Err(carina_core::provider::ProviderError::invalid_input(
            "provider rejected sibling",
        ))
    }
    fn create_provider(
        &self,
        _: &carina_core::provider::ProviderSession,
        _: carina_core::provider::InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<carina_core::provider::ProviderInitialization>> {
        unreachable!("validation does not create providers")
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        Vec::new()
    }
    fn schema_witness(&self) -> carina_core::schema::ValidatedProviderSchemaWitness {
        carina_core::schema::ValidatedProviderSchemaWitness::new(
            Vec::new(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
}

#[test]
fn issue_4276_round1_provider_config_keeps_main_local_error_text() {
    let fixture = write_fixture(&[
        (
            "providers.crn",
            "provider awsccmock { params = [\"abcd\", \"bad\"] }\n",
        ),
        ("main.crn", ""),
    ]);
    let cli = single_value_walk_cli_errors(vec![Box::new(Round1ConfigFactory)], &fixture);
    assert_eq!(
        cli,
        vec![
            "provider awsccmock: params: List item at index 0: Invalid VpcId value 'abcd': length 4 is outside allowed range [, 3]"
        ]
    );
    let diagnostics =
        single_value_walk_lsp_diagnostics(vec![Box::new(Round1ConfigFactory)], &fixture);
    assert!(
        diagnostics.is_empty(),
        "the provider diagnostic belongs to providers.crn: {diagnostics:#?}"
    );
    let factories: Vec<Box<dyn ProviderFactory>> = vec![Box::new(Round1ConfigFactory)];
    let loaded = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(loaded), vec!["awsccmock".into()]);
    let diagnostics = lsp_diagnostics(&engine, &fixture, "providers.crn");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "provider awsccmock: params: List item at index 0: Invalid VpcId value 'abcd': length 4 is outside allowed range [, 3]"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 30), Position::new(0, 45))
    );
}

fn single_value_walk_m01_rejects_union(cli: bool) {
    let diagnostics = if cli {
        single_value_walk_cli_errors
    } else {
        single_value_walk_lsp_errors
    };
    let fixture = single_value_walk_m01_fixture();
    eprintln!("M01 CONTROL scalar provider rejection");
    let control = diagnostics(
        single_value_walk_m01_factories(single_value_walk_m01_vpc_id_type()),
        &fixture,
    );
    let message = "awsccmock rejected `VpcId`: Invalid vpc_id 'bad': must start with 'vpc-'";
    let expected = if cli {
        format!("awsccmock.test.Sink.<pending>: {message}")
    } else {
        message.to_string()
    };
    assert_eq!(control, vec![expected], "M01 scalar control");

    eprintln!("M01 TARGET Union<Int,VpcId>");
    let errors = diagnostics(
        single_value_walk_m01_factories(AttributeType::union(vec![
            AttributeType::int(),
            single_value_walk_m01_vpc_id_type(),
        ])),
        &fixture,
    );
    assert_eq!(
        errors, control,
        "M01 must retain exactly the scalar provider rejection"
    );
}

#[test]
fn single_value_walk_m01_cli_rejects_union_without_valid_arm() {
    single_value_walk_m01_rejects_union(true);
}

#[test]
fn single_value_walk_m01_lsp_rejects_union_without_valid_arm() {
    single_value_walk_m01_rejects_union(false);
}

#[test]
fn single_value_walk_union_accepts_an_arm_that_passes_both_stages() {
    let fixture = single_value_walk_m01_fixture();
    for value in ["bad", "vpc-valid"] {
        std::fs::write(
            fixture.path().join("main.crn"),
            format!("awsccmock.test.Sink {{ params = \"{value}\" }}\n"),
        )
        .unwrap();
        for diagnostics in [single_value_walk_cli_errors, single_value_walk_lsp_errors] {
            let errors = diagnostics(
                single_value_walk_m01_factories(AttributeType::union(vec![
                    single_value_walk_m01_vpc_id_type(),
                    AttributeType::string(),
                ])),
                &fixture,
            );
            assert!(
                errors.is_empty(),
                "String accepts the complete value: {errors:#?}"
            );
        }
    }
}

fn single_value_walk_nested_fixture(with_sibling: bool) -> (TempDir, AttributeType) {
    let main = if with_sibling {
        r#"awsccmock.test.Sink { params = [{ entries = { bad = "abcd", sibling = "bad" } }] }"#
    } else {
        r#"awsccmock.test.Sink { params = [{ entries = { bad = "abcd" } }] }"#
    };
    let fixture = write_fixture(&[
        (
            "providers.crn",
            include_str!("fixtures/single_value_walk/m01_union_provider/providers.crn"),
        ),
        ("main.crn", main),
    ]);
    let leaf = AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(3))),
        None,
    );
    let params = AttributeType::list(AttributeType::struct_(
        "Entry",
        vec![StructField::optional("entries", AttributeType::map(leaf)).build()],
    ));
    (fixture, params)
}

#[test]
fn single_value_walk_nested_local_failure_skips_provider_rejection() {
    let (fixture, params) = single_value_walk_nested_fixture(false);
    for diagnostics in [
        single_value_walk_cli_errors,
        single_value_walk_lsp_value_findings,
    ] {
        let errors = diagnostics(single_value_walk_m01_factories(params.clone()), &fixture);
        assert_eq!(errors.len(), 1, "one nested local failure: {errors:#?}");
        assert!(
            errors[0].contains("length 4 is outside allowed range [, 3]"),
            "{errors:#?}"
        );
        assert!(!errors[0].contains("rejected `VpcId`"), "{errors:#?}");
    }
}

#[test]
fn single_value_walk_valid_sibling_still_reports_provider_rejection() {
    let (fixture, params) = single_value_walk_nested_fixture(true);
    for diagnostics in [
        single_value_walk_cli_errors,
        single_value_walk_lsp_value_findings,
    ] {
        let errors = diagnostics(single_value_walk_m01_factories(params.clone()), &fixture);
        assert_eq!(
            errors.len(),
            2,
            "both sibling failures, once each: {errors:#?}"
        );
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.contains("length 4 is outside allowed range [, 3]"))
                .count(),
            1,
            "{errors:#?}"
        );
        assert_eq!(
            errors
                .iter()
                .filter(|error| error.contains("must start with 'vpc-'"))
                .count(),
            1,
            "{errors:#?}"
        );
    }
}

fn wasm_style_flow_log_schema() -> ResourceSchema {
    let identity = carina_core::schema::TypeIdentity::new(
        Some("aws"),
        ["ec2", "FlowLog"],
        "MaxAggregationInterval",
    );
    ResourceSchema::new("ec2.FlowLog").attribute(
        AttributeSchema::optional(
            "max_aggregation_interval",
            AttributeType::refined_int(Some(identity), None),
        )
        .build(),
    )
}

#[test]
fn wasm_plugin_custom_validator_rejects_invalid_vpc_id_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.security_group {
    group_description = "web sg"
    vpc_id            = "test"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        lsp_messages_contain(&lsp_diags, "vpc_id") || lsp_messages_contain(&lsp_diags, "vpc-"),
        "LSP must diagnose invalid vpc_id, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "vpc_id") || cli_messages_contain(&cli_diags, "vpc-"),
        "CLI must diagnose invalid vpc_id, got {:?}",
        cli_diags,
    );
}

#[test]
fn wasm_plugin_custom_validator_accepts_valid_vpc_id_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.security_group {
    group_description = "web sg"
    vpc_id            = "vpc-12345678"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        !lsp_messages_contain(&lsp_diags, "vpc-"),
        "LSP must not diagnose a valid vpc_id, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain(&cli_diags, "vpc-"),
        "CLI must not diagnose a valid vpc_id, got {:?}",
        cli_diags,
    );
}

#[test]
fn numeric_wasm_validator_accepts_60_in_cli_and_lsp() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.FlowLog {
    max_aggregation_interval = 60
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        !lsp_messages_contain(&lsp_diags, "max_aggregation_interval"),
        "LSP must accept numeric bridge value 60, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain(&cli_diags, "max_aggregation_interval"),
        "CLI validate must accept numeric bridge value 60, got {cli_diags:?}",
    );
}

#[test]
fn numeric_wasm_validator_rejects_61_in_cli_and_lsp() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.FlowLog {
    max_aggregation_interval = 61
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&wasm_style_engine(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_factories(), &fixture);

    assert!(
        lsp_messages_contain(&lsp_diags, "max_aggregation_interval"),
        "LSP must reject numeric bridge value 61, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain(&cli_diags, "max_aggregation_interval"),
        "CLI validate must reject numeric bridge value 61, got {cli_diags:?}",
    );
}

fn wasm_style_subnet_schema() -> ResourceSchema {
    let vpc_id_type = AttributeType::refined_string(
        Some(carina_core::schema::TypeIdentity::bare("VpcId")),
        None,
        None,
        None,
    );
    ResourceSchema::new("ec2.subnet")
        .attribute(AttributeSchema::optional("vpc_ids", AttributeType::list(vpc_id_type)).build())
}

fn wasm_style_subnet_factories() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awsccmock".to_string(),
        schemas: vec![
            wasm_style_subnet_schema()
                .validate()
                .expect("factory test schema must be valid"),
        ],
    }) as Box<dyn ProviderFactory>]
}

#[test]
fn wasm_plugin_custom_validator_rejects_invalid_vpc_id_inside_list_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.subnet {
    vpc_ids = ["vpc-good", "bogus"]
}
"#,
    )]);
    let provider_names = vec!["awsccmock".to_string()];
    let factories = wasm_style_subnet_factories();
    let loaded_providers = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    let engine = DiagnosticEngine::new(Arc::new(loaded_providers), provider_names);

    let lsp_diags = lsp_diagnostics(&engine, &fixture, "main.crn");
    let cli_diags = cli_diagnostics(wasm_style_subnet_factories(), &fixture);

    assert!(
        cli_messages_contain(&cli_diags, "bogus") || cli_messages_contain(&cli_diags, "vpc-"),
        "CLI must diagnose invalid vpc_id inside list, got {:?}",
        cli_diags,
    );
    assert!(
        lsp_messages_contain(&lsp_diags, "bogus") || lsp_messages_contain(&lsp_diags, "vpc-"),
        "LSP must diagnose invalid vpc_id inside list, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn wasm_plugin_custom_validator_reports_every_bad_item_in_list() {
    // Multi-item failure case: walk_custom_lookup should accumulate one
    // diagnostic per bad item instead of stopping at the first.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awsccmock.ec2.subnet {
    vpc_ids = ["bad-1", "bad-2"]
}
"#,
    )]);
    let cli_diags = cli_diagnostics(wasm_style_subnet_factories(), &fixture);
    let bad_count = cli_diags
        .iter()
        .filter(|m| m.contains("bad-1") || m.contains("bad-2"))
        .count();
    assert_eq!(
        bad_count, 2,
        "CLI must report a diagnostic per bad list item, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario: directional generic/specific assignments (#2358)
//
// `vpc_id: String = main.vpc_id` declares a `String`-typed export of a
// value whose actual type is `Custom { identity: VpcId }`. The
// declaration is a safe widening and therefore passes. A downstream
// consumer sees only `String`, so the reverse flow into a specific
// `Custom { VpcId }` sink remains rejected. Validation, LSP diagnostics,
// and completion must agree on that directionality.
// ============================================================================

// `vpc-` prefix validator — mirrors how the awscc plugin attaches its
// real `prefixed` validators. Used by #2358 fixtures so the literal-
// assignment test can prove the schema-attached `validate` closure
// actually runs (a noop validator would let the test pass for the
// wrong reason).
fn vpc_id_validate_2358(v: &Value) -> Result<(), String> {
    match v {
        Value::Concrete(ConcreteValue::String(s)) if s.expose_plaintext().starts_with("vpc-") => {
            Ok(())
        }
        Value::Concrete(ConcreteValue::String(s)) => Err(format!(
            "Invalid vpc_id '{}': must start with 'vpc-'",
            s.expose_plaintext()
        )),
        _ => Ok(()),
    }
}

fn vpc_id_custom_type_2358() -> AttributeType {
    AttributeType::refined_string_with_validator(
        carina_core::schema::TypeIdentity::bare("VpcId"),
        None,
        None,
        legacy_validator(move |value| vpc_id_validate_2358(value).map_err(Into::into)),
        None,
    )
}

fn vpc_resource_schema_2358() -> ResourceSchema {
    ResourceSchema::new("ec2.Vpc")
        .attribute(AttributeSchema::optional("cidr_block", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_custom_type_2358()).build())
}

fn security_group_schema_2358() -> ResourceSchema {
    ResourceSchema::new("ec2.SecurityGroup")
        .attribute(AttributeSchema::optional("group_description", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("vpc_id", vpc_id_custom_type_2358()).build())
}

fn factories_2358() -> Vec<Box<dyn ProviderFactory>> {
    vec![Box::new(WasmStyleProviderFactory {
        name: "awscc2358".to_string(),
        schemas: vec![
            vpc_resource_schema_2358()
                .validate()
                .expect("factory test schema must be valid"),
            security_group_schema_2358()
                .validate()
                .expect("factory test schema must be valid"),
        ],
    }) as Box<dyn ProviderFactory>]
}

fn engine_2358() -> DiagnosticEngine {
    let factories = factories_2358();
    let loaded_providers = carina_core::provider::loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        carina_core::EnvironmentSnapshot::empty(),
    );
    DiagnosticEngine::new(Arc::new(loaded_providers), vec!["awscc2358".to_string()])
}

#[test]
fn export_string_annotation_of_custom_value_widens_parity() {
    // The export declares `: String` but the rhs `main.vpc_id` carries
    // type `Custom { VpcId }`. This is source-specific → sink-generic,
    // so both validate and LSP diagnostics must accept the widening.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id: String = main.vpc_id
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "mismatch"),
        "LSP must accept Custom-to-String widening, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "CLI must accept Custom-to-String widening, got {:?}",
        cli_diags,
    );
}

#[test]
fn secret_valued_exports_are_rejected_with_cli_lsp_parity() {
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let s = secret("topsecretXYZ")

exports {
    direct: String = s
    builtin: String = upper(s)
    interpolated: String = "x-${s}"
    inline_interpolated = "x-${secret("topsecretXYZ")}"
    nested_list: list(String) = [s]
    nested_map: map(String) = { key = s }
}
"#,
    )]);
    let engine = engine_with_schemas(SchemaRegistry::new());

    let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
    let cli = cli_diagnostics(vec![], &fixture);

    for name in [
        "direct",
        "builtin",
        "interpolated",
        "inline_interpolated",
        "nested_list",
        "nested_map",
    ] {
        let expected = format!("export '{name}' cannot hold a secret value");
        assert!(
            lsp.iter().any(|diagnostic| diagnostic.message == expected),
            "LSP missing {expected:?}: {lsp:#?}",
        );
        assert!(
            cli.iter().any(|diagnostic| diagnostic.contains(&expected)),
            "CLI missing {expected:?}: {cli:#?}",
        );
    }
    assert!(
        lsp.iter()
            .all(|diagnostic| !diagnostic.message.contains("topsecretXYZ")),
        "LSP leaked secret: {lsp:#?}",
    );
    assert!(
        cli.iter()
            .all(|diagnostic| !diagnostic.contains("topsecretXYZ")),
        "CLI leaked secret: {cli:#?}",
    );
}

#[test]
fn export_vpcid_annotation_of_custom_value_accepted_parity() {
    // Safe direction: the export declares `: VpcId`, matching the
    // rhs's actual type. Both validate and LSP must stay quiet.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id: VpcId = main.vpc_id
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "mismatch"),
        "LSP must not flag a same-type export as a mismatch, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "CLI must not flag a same-type export as a mismatch, got {:?}",
        cli_diags,
    );
}

#[test]
fn literal_valid_string_assignment_to_custom_receiver_still_validates() {
    // Existing `awscc.ec2.SecurityGroup { vpc_id = 'vpc-12345678' }`
    // pattern remains valid: a literal string in source becomes
    // `Value::Concrete(ConcreteValue::String(...))`, which validation handles via the schema-
    // attached `validate` closure on the Custom type — not via the
    // lifted module-boundary relation. The strictness fix must
    // not regress this path.
    //
    // The fixture's validator requires a `vpc-` prefix; a well-formed
    // value must pass. Paired with the malformed-literal test below
    // this proves the `validate` closure is actually being run (a
    // silent noop validator would let either test pass for the wrong
    // reason).
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awscc2358.ec2.SecurityGroup {
    group_description = "web sg"
    vpc_id            = "vpc-12345678"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "vpc_id"),
        "LSP must not flag a well-formed literal vpc_id, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "vpc_id"),
        "CLI must not flag a well-formed literal vpc_id, got {:?}",
        cli_diags,
    );
}

#[test]
fn literal_malformed_string_assignment_still_runs_attached_validator() {
    // Companion to the well-formed test above. A malformed literal
    // (`'bad'` — no `vpc-` prefix) must trip the schema-attached
    // `validate` closure and surface in both surfaces. Together the
    // pair proves the literal-assignment path is genuinely intact:
    // the well-formed test is meaningful only because this one shows
    // the validator can fail.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
awscc2358.ec2.SecurityGroup {
    group_description = "web sg"
    vpc_id            = "bad"
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        lsp_messages_contain_ci(&lsp_diags, "vpc-"),
        "LSP must surface the vpc_id validator's prefix complaint, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        cli_messages_contain_ci(&cli_diags, "vpc-"),
        "CLI must surface the vpc_id validator's prefix complaint, got {:?}",
        cli_diags,
    );
}

// ============================================================================
// Scenario: rhs-driven inference for unannotated exports (#2361, stage 1
// of #2360)
//
// Closes the unannotated half of the soundness hole #2358 / PR #2359
// only partially closed: previously `exports { vpc_id = main.vpc_id }`
// (no annotation) was skipped by every type-checking predicate, so a
// downstream `vpc_id = network.vpc_id` against a `Custom{VpcId}`
// receiver passed through unchecked. After this stage, the inferer
// fills in the missing `type_expr` from the rhs whenever it can; if
// it cannot (e.g. dynamic builtin, heterogeneous list), validation
// surfaces a "type annotation required" error.
// ============================================================================

#[test]
fn unannotated_export_of_resource_ref_inferred_and_typechecked() {
    // `vpc_id = main.vpc_id` infers `VpcId` from the `Vpc` schema,
    // which makes the export carry that type even though the user
    // wrote no annotation. Both surfaces stay quiet.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id = main.vpc_id
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);

    assert!(
        !lsp_messages_contain_ci(&lsp_diags, "mismatch"),
        "LSP must not flag a same-type unannotated export, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "CLI must not flag a same-type unannotated export, got {:?}",
        cli_diags,
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "annotation required"),
        "CLI must infer the type and not demand annotation, got {:?}",
        cli_diags,
    );
}

#[test]
fn unannotated_export_of_dynamic_lookup_demands_annotation() {
    // `lookup` is a `BuiltinResultTyping::Any` builtin — the return type
    // depends on arguments. Unannotated, the inferer cannot determine
    // a static type and must surface "type annotation required".
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
exports {
    zone_id = lookup({a = "1"}, "a", "default")
}
"#,
    )]);

    let lsp_diags = lsp_diagnostics(&engine_2358(), &fixture, "main.crn");
    let cli_diags = cli_diagnostics(factories_2358(), &fixture);
    assert!(
        cli_messages_contain_ci(&cli_diags, "annotation required"),
        "CLI must demand a type annotation for an Any-returning builtin, got {:?}",
        cli_diags,
    );
    assert!(
        cli_messages_contain_ci(&cli_diags, "zone_id"),
        "diagnostic must name the offending export, got {:?}",
        cli_diags,
    );
    // validate / LSP parity: the same diagnostic must surface in-editor.
    assert!(
        lsp_messages_contain_ci(&lsp_diags, "annotation required"),
        "LSP must demand a type annotation for an Any-returning builtin, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
    assert!(
        lsp_messages_contain_ci(&lsp_diags, "zone_id"),
        "LSP diagnostic must name the offending export, got {:?}",
        lsp_diags.iter().map(|d| &d.message).collect::<Vec<_>>(),
    );
}

#[test]
fn unannotated_export_of_literal_inferred_as_primitive() {
    // A literal-only rhs is inferable to its primitive type. The
    // export typechecks without annotation and without spurious
    // diagnostics.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
exports {
    name = "carina"
    count = 3
    enabled = true
}
"#,
    )]);

    let cli_diags = cli_diagnostics(factories_2358(), &fixture);
    assert!(
        !cli_messages_contain_ci(&cli_diags, "annotation required"),
        "literals must infer cleanly, got {:?}",
        cli_diags,
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "literals must not flag a mismatch, got {:?}",
        cli_diags,
    );
}

#[test]
fn explicit_annotation_is_not_overwritten_by_inference() {
    // With an explicit `: VpcId` the export keeps that declared type
    // even though the rhs is also `Custom{VpcId}` — confirm inference
    // doesn't race against user intent.
    let fixture = write_fixture(&[(
        "main.crn",
        r#"
let main = awscc2358.ec2.Vpc {
    cidr_block = "10.0.0.0/16"
}

exports {
    vpc_id: VpcId = main.vpc_id
}
"#,
    )]);

    let cli_diags = cli_diagnostics(factories_2358(), &fixture);
    assert!(
        !cli_messages_contain_ci(&cli_diags, "mismatch"),
        "annotated same-type export must stay clean, got {:?}",
        cli_diags,
    );
    assert!(
        !cli_messages_contain_ci(&cli_diags, "annotation required"),
        "annotation is present — must not demand one, got {:?}",
        cli_diags,
    );
}

#[test]
fn declared_export_unknown_binding_reports_exactly_one_diagnostic_per_frontend() {
    let fixture = write_fixture(&[(
        "main.crn",
        "exports {\n  x: String = typo_binding.attr\n}\n",
    )]);
    let schemas = SchemaRegistry::new();

    let lsp = lsp_diagnostics(&engine_with_schemas(schemas.clone()), &fixture, "main.crn");
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);

    assert_eq!(
        lsp.len(),
        1,
        "LSP must report the identifier-scope error once: {lsp:#?}",
    );
    assert_eq!(
        cli.len(),
        1,
        "CLI must report the identifier-scope error once: {cli:#?}",
    );
    assert!(
        lsp[0].message.contains("Undefined") && lsp[0].message.contains("typo_binding"),
        "unexpected LSP diagnostic: {lsp:#?}",
    );
    assert!(
        cli[0].contains("Undefined") && cli[0].contains("typo_binding"),
        "unexpected CLI diagnostic: {cli:#?}",
    );
}

// ============================================================================
// Integer arithmetic: directory-wide CLI/LSP parity (carina#4142)
// ============================================================================

fn integer_arithmetic_directory_schemas() -> SchemaRegistry {
    let source = ResourceSchema::new("r.Source")
        .attribute(AttributeSchema::provider_populated("seconds", AttributeType::int()).build())
        .attribute(AttributeSchema::provider_populated("ratio", AttributeType::float()).build());
    let consumer = ResourceSchema::new("r.Consumer")
        .attribute(AttributeSchema::required("concrete", AttributeType::int()).build())
        .attribute(AttributeSchema::required("deferred", AttributeType::int()).build())
        .attribute(AttributeSchema::required("widened", AttributeType::float()).build())
        .attribute(AttributeSchema::required("label", AttributeType::string()).build());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source.validate().expect("source schema must be valid"),
    );
    schemas.insert(
        "test",
        consumer.validate().expect("consumer schema must be valid"),
    );
    schemas
}

fn integer_arithmetic_directory_fixture(
    concrete_expression: &str,
    label_expression: &str,
    extra_main: &str,
) -> TempDir {
    let main = format!(
        "let sink = test.r.Consumer {{\n  concrete = {concrete_expression}\n  deferred = source.seconds / 60\n  widened = source.seconds / 60\n  label = \"wait ${{{label_expression}}} minutes\"\n}}\n{extra_main}"
    );
    write_fixture(&[
        ("main.crn", &main),
        (
            "exports.crn",
            "let minutes = base_seconds / 60\n\
             let doubled_minutes = minutes * 2\n\
             exports {\n  minutes: Int = doubled_minutes\n}\n",
        ),
        (
            "providers.crn",
            "provider test {}\nlet source = test.r.Source {}\n",
        ),
        (
            "backend.crn",
            "backend local { path = \"carina.state.json\" }\nlet base_seconds = 120\n",
        ),
    ])
}

#[test]
fn integer_arithmetic_directory_has_exact_cli_lsp_diagnostic_parity() {
    struct Case {
        name: &'static str,
        expression: &'static str,
        label_expression: &'static str,
        extra_main: &'static str,
        expected: Option<&'static str>,
        operator_line: u32,
        operator_prefix: &'static str,
        operator_expression: &'static str,
    }

    for case in [
        Case {
            name: "success",
            expression: "doubled_minutes + 1",
            label_expression: "source.seconds / 60",
            extra_main: "",
            expected: None,
            operator_line: 1,
            operator_prefix: "  concrete = ",
            operator_expression: "doubled_minutes + 1",
        },
        Case {
            name: "type error",
            expression: "doubled_minutes + \"bad\"",
            label_expression: "source.seconds / 60",
            extra_main: "",
            expected: Some("operator '+': right operand must be Int, got String"),
            operator_line: 1,
            operator_prefix: "  concrete = ",
            operator_expression: "doubled_minutes + \"bad\"",
        },
        Case {
            name: "typed deferred Float operand",
            expression: "source.ratio + 1",
            label_expression: "source.seconds / 60",
            extra_main: "",
            expected: Some("operator '+': left operand must be Int, got Float"),
            operator_line: 1,
            operator_prefix: "  concrete = ",
            operator_expression: "source.ratio + 1",
        },
        Case {
            name: "typed deferred Float operand in interpolation",
            expression: "doubled_minutes + 1",
            label_expression: "source.ratio + 1",
            extra_main: "",
            expected: Some("operator '+': left operand must be Int, got Float"),
            operator_line: 4,
            operator_prefix: "  label = \"wait ${",
            operator_expression: "source.ratio + 1",
        },
        Case {
            name: "division by zero",
            expression: "doubled_minutes / 0",
            label_expression: "source.seconds / 60",
            extra_main: "",
            expected: Some("operator '/': integer division by zero"),
            operator_line: 1,
            operator_prefix: "  concrete = ",
            operator_expression: "doubled_minutes / 0",
        },
        Case {
            name: "overflow",
            expression: "9223372036854775807 + 1",
            label_expression: "source.seconds / 60",
            extra_main: "",
            expected: Some("operator '+': integer arithmetic overflow"),
            operator_line: 1,
            operator_prefix: "  concrete = ",
            operator_expression: "9223372036854775807 + 1",
        },
        Case {
            name: "unused let with typed deferred Float operand",
            expression: "doubled_minutes + 1",
            label_expression: "source.seconds / 60",
            extra_main: "let invalid = source.ratio + 1\n",
            expected: Some("operator '+': left operand must be Int, got Float"),
            operator_line: 6,
            operator_prefix: "let invalid = ",
            operator_expression: "source.ratio + 1",
        },
    ] {
        let fixture = integer_arithmetic_directory_fixture(
            case.expression,
            case.label_expression,
            case.extra_main,
        );
        let schemas = integer_arithmetic_directory_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        let lsp_errors = lsp
            .iter()
            .filter(|diagnostic| {
                diagnostic.severity == Some(tower_lsp::lsp_types::DiagnosticSeverity::ERROR)
            })
            .collect::<Vec<_>>();

        match case.expected {
            None => {
                assert!(cli.is_empty(), "{} CLI diagnostics: {cli:#?}", case.name);
                assert!(
                    lsp_errors.is_empty(),
                    "{} LSP diagnostics: {lsp:#?}",
                    case.name
                );
            }
            Some(expected) => {
                assert_eq!(cli.len(), 1, "{} CLI diagnostics: {cli:#?}", case.name);
                assert_eq!(
                    lsp_errors.len(),
                    1,
                    "{} LSP diagnostics: {lsp:#?}",
                    case.name
                );
                assert!(
                    cli[0].contains(expected),
                    "{} CLI diagnostic must contain {expected:?}: {cli:#?}",
                    case.name
                );
                assert_eq!(
                    lsp_errors[0].message, expected,
                    "{} must use the core diagnostic verbatim",
                    case.name
                );
                let operator_column = case.operator_prefix.len()
                    + case
                        .operator_expression
                        .find(['+', '-', '*', '/', '%'])
                        .expect("error case has an arithmetic operator");
                assert_eq!(
                    lsp_errors[0].range,
                    tower_lsp::lsp_types::Range::new(
                        tower_lsp::lsp_types::Position::new(
                            case.operator_line,
                            operator_column as u32,
                        ),
                        tower_lsp::lsp_types::Position::new(
                            case.operator_line,
                            operator_column as u32 + 1,
                        ),
                    ),
                    "{} must underline the arithmetic operator",
                    case.name
                );
            }
        }
    }
}

#[test]
fn integer_arithmetic_directory_diagnostics_map_operator_after_heredoc() {
    let main = "let p = <<EOT\na\nb\nc\nd\nEOT\n\nlet x = 1 / 0\n";
    let fixture = write_fixture(&[("main.crn", main)]);
    let cli = cli_diagnostics(Vec::new(), &fixture);
    let lsp = lsp_diagnostics(
        &DiagnosticEngine::without_loaded_provider_schemas(
            carina_core::EnvironmentSnapshot::empty(),
        ),
        &fixture,
        "main.crn",
    );
    let arithmetic = lsp
        .iter()
        .find(|diagnostic| diagnostic.message.contains("integer division by zero"))
        .unwrap_or_else(|| panic!("missing LSP arithmetic diagnostic: {lsp:#?}"));

    assert_eq!(cli.len(), 1, "CLI diagnostics: {cli:#?}");
    assert!(
        cli[0].contains("line 8, column 11"),
        "CLI must report the authored operator position: {cli:#?}",
    );
    assert_eq!(
        arithmetic.range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(7, 10),
            tower_lsp::lsp_types::Position::new(7, 11),
        ),
    );
}

#[test]
fn integer_arithmetic_directory_maps_errors_on_both_sides_of_heredoc() {
    let main = "provider test {}\n\
                let source = test.r.Source {}\n\
                let before = source.ratio + 1\n\
                let p = <<EOT\n\
                a\n\
                b\n\
                c\n\
                EOT\n\
                let after = source.ratio / 1\n";
    let fixture = write_fixture(&[("main.crn", main)]);
    let schemas = integer_arithmetic_directory_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let arithmetic = lsp
        .iter()
        .filter(|diagnostic| {
            diagnostic
                .message
                .contains("operand must be Int, got Float")
        })
        .collect::<Vec<_>>();

    assert_eq!(cli.len(), 2, "CLI diagnostics: {cli:#?}");
    assert!(
        cli.iter()
            .any(|diagnostic| diagnostic.contains("operator '+'")),
        "CLI must retain the first arithmetic diagnostic: {cli:#?}",
    );
    assert!(
        cli.iter()
            .any(|diagnostic| diagnostic.contains("operator '/'")),
        "CLI must retain the arithmetic diagnostic after the heredoc: {cli:#?}",
    );
    let mut ranges = arithmetic
        .iter()
        .map(|diagnostic| diagnostic.range)
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| (range.start.line, range.start.character));
    assert_eq!(
        ranges,
        vec![
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(2, 26),
                tower_lsp::lsp_types::Position::new(2, 27),
            ),
            tower_lsp::lsp_types::Range::new(
                tower_lsp::lsp_types::Position::new(8, 25),
                tower_lsp::lsp_types::Position::new(8, 26),
            ),
        ],
        "LSP must underline both authored operators: {lsp:#?}",
    );
}

#[path = "e2e_typecheck_parity/review_4282.rs"]
mod review_4282;

#[path = "e2e_typecheck_parity/review_round1_4282.rs"]
mod review_round1_4282;

#[path = "e2e_typecheck_parity/review_round2_4282.rs"]
mod review_round2_4282;

#[test]
fn pattern_modes_directory_validate_and_lsp_parity() {
    for (attribute, value, accepted) in [
        ("full", "xabcx", false),
        ("substring", "xabcx", true),
        ("substring", "xyz", false),
        ("full", "abc", true),
    ] {
        let source =
            format!("let patterned = mock.test.pattern_modes {{ {attribute} = '{value}' }}\n");
        let exports = format!("exports {{ marker = patterned.{attribute} }}\n");
        let fixture = write_fixture(&[
            ("providers.crn", "provider mock {}\n"),
            ("main.crn", &source),
            ("exports.crn", &exports),
        ]);
        let schemas = carina_provider_mock::builtin_schemas().into_iter().fold(
            SchemaRegistry::new(),
            |mut registry, schema| {
                registry.insert("mock", schema);
                registry
            },
        );
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
            .args(["validate", fixture.path().to_str().unwrap(), "--json"])
            .env("NO_COLOR", "1")
            .output()
            .expect("run production validate binary");
        assert_eq!(
            output.status.success(),
            accepted,
            "{attribute}={value}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert_eq!(cli.is_empty(), accepted, "{attribute}={value}: {cli:#?}");
        assert_eq!(lsp.is_empty(), accepted, "{attribute}={value}: {lsp:#?}");
        if !accepted {
            assert_eq!(cli.len(), 1, "{cli:#?}");
            assert_eq!(lsp.len(), 1, "{lsp:#?}");
            assert_eq!(lsp[0].severity, Some(DiagnosticSeverity::ERROR));
            assert!(
                cli[0].contains("does not match required pattern"),
                "{cli:#?}"
            );
            assert!(
                cli[0].contains(&lsp[0].message),
                "CLI={cli:#?}, LSP={lsp:#?}"
            );
        }
    }
}

#[test]
fn pattern_reason_controls_are_escaped_in_cli_and_lsp_diagnostics() {
    use carina_core::schema::{PatternMatch, PatternReason, PatternSpec, TypeIdentity};

    let reason = "prefix-only\n\u{1b}[31m metadata";
    let spec = PatternSpec::new(
        "abc",
        PatternMatch::Substring {
            reason: PatternReason::new(reason).unwrap(),
        },
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("r.pattern")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::refined_string(
                        Some(TypeIdentity::new(Some("test"), ["r"], "PatternText")),
                        Some(spec),
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = engine_with_schemas(schemas.clone());
    for source in [
        "test.r.pattern { value = 'bad' }\n",
        "test.r.pattern { value = \"${'bad'}\" }\n",
    ] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("main.crn", source),
            ("exports.crn", "exports { marker = 'reason diagnostic' }\n"),
        ]);
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
        assert_eq!(cli.len(), 1, "{source}: {cli:#?}");
        assert_eq!(lsp.len(), 1, "{source}: {lsp:#?}");
        assert_eq!(
            lsp[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "{source}: {lsp:#?}"
        );
        assert!(
            cli[0].contains(&lsp[0].message),
            "CLI={cli:#?}, LSP={lsp:#?}"
        );
        for diagnostic in [cli[0].as_str(), lsp[0].message.as_str()] {
            assert!(
                diagnostic.contains("prefix-only\\n\\u{1b}[31m metadata"),
                "{diagnostic:?}"
            );
            assert!(!diagnostic.contains(reason), "{diagnostic:?}");
            assert!(!diagnostic.contains('\u{1b}'), "{diagnostic:?}");
        }
    }
}

fn round2_attribute_fixture(name: &str) -> TempDir {
    let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/value_knowledge")
        .join(name);
    let fixture = tempfile::tempdir().unwrap();
    for entry in std::fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), fixture.path().join(entry.file_name())).unwrap();
    }
    fixture
}

fn round2_attribute_schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated("identifier", AttributeType::string()).build(),
            )
            .attribute(AttributeSchema::optional("parent_name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("comment", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                    .build(),
            )
            .with_unique_name_attribute("name")
            .validate()
            .unwrap(),
    );
    schemas
}

fn assert_round2_attribute_siblings(fixture: &TempDir) {
    let schemas = round2_attribute_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), fixture, "main.crn");
    let errors: Vec<_> = lsp
        .iter()
        .filter(|d| {
            matches!(
                d.severity,
                Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)
            )
        })
        .collect();
    assert_eq!(cli.len(), 2, "CLI: {cli:#?}");
    assert_eq!(errors.len(), cli.len(), "LSP: {errors:#?}\nCLI: {cli:#?}");
    for cause in ["expected Map<String>, got Int", "expected String, got Bool"] {
        assert!(
            cli.iter().any(|message| message.contains(cause)),
            "{cli:#?}"
        );
        assert!(
            errors.iter().any(|finding| finding.message.contains(cause)),
            "{errors:#?}"
        );
    }
}

#[test]
fn failed_secret_attribute_matches_unwrapped_strict_cli_and_lsp_findings() {
    const MISSING: &str = "CARINA_KNOWLEDGE_MISSING_INPUT";
    for protected in [false, true] {
        let fixture = round2_attribute_fixture("multi_attribute");
        if !protected {
            let path = fixture.path().join("main.crn");
            let text = std::fs::read_to_string(&path).unwrap().replace(
                &format!("secret(env(\"{MISSING}\"))"),
                &format!("env(\"{MISSING}\")"),
            );
            std::fs::write(path, text).unwrap();
        }
        let schemas = round2_attribute_schemas();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        let cause = if protected {
            carina_core::resource::INSIDE_SECRET_MESSAGE.to_string()
        } else {
            format!("env(): environment variable '{MISSING}' is not set")
        };
        let strict_cause = format!("Invalid expression at line 3: {cause}");
        assert_eq!(
            cli,
            [format!(
                "{}:1:1: {strict_cause}",
                fixture.path().join("main.crn").display()
            )],
        );
        let actual: Vec<_> = lsp
            .iter()
            .map(|finding| (finding.severity, finding.message.as_str()))
            .collect();
        assert_eq!(
            actual,
            [
                (Some(DiagnosticSeverity::ERROR), cause.as_str()),
                (Some(DiagnosticSeverity::ERROR), strict_cause.as_str()),
                (
                    Some(DiagnosticSeverity::WARNING),
                    "Type mismatch: expected String, got Bool"
                ),
                (
                    Some(DiagnosticSeverity::WARNING),
                    "Type mismatch: expected Map<String>, got Int"
                ),
            ]
        );
        let root_range = Range::new(Position::new(0, 0), Position::new(5, 1));
        assert_eq!(
            lsp.iter()
                .map(|finding| (finding.range, finding.source.as_deref()))
                .collect::<Vec<_>>(),
            [
                (root_range, Some("carina")),
                (root_range, Some("carina")),
                (
                    Range::new(Position::new(3, 2), Position::new(3, 9)),
                    Some("carina"),
                ),
                (
                    Range::new(Position::new(4, 2), Position::new(4, 6)),
                    Some("carina"),
                ),
            ],
        );
        // carina#4337: main also reports the failing site twice; preserve that count here.
        assert_eq!(
            lsp.iter()
                .filter(|finding| finding.severity == Some(DiagnosticSeverity::ERROR))
                .count(),
            2
        );
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
            .current_dir(fixture.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .env_remove(MISSING)
            .args(["validate", "."])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty(), "{:?}", output.stdout);
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            format!("Error: ./main.crn:1:1: {strict_cause}\n")
        );
        if protected {
            let rendered = format!(
                "{cli:?} {lsp:?} {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(rendered.contains(carina_core::resource::INSIDE_SECRET_MESSAGE));
            assert!(!rendered.contains(MISSING), "{rendered}");
            assert!(!rendered.contains("secret(env("), "{rendered}");
        }
    }
}

#[test]
fn invalid_sibling_attributes_are_both_reported_without_failed_input() {
    let fixture = round2_attribute_fixture("multi_attribute");
    let path = fixture.path().join("main.crn");
    let text = std::fs::read_to_string(&path).unwrap().replace(
        r#""x-${secret(env("CARINA_KNOWLEDGE_MISSING_INPUT"))}""#,
        r#""available-parent""#,
    );
    assert!(!text.contains("env("), "{text}");
    std::fs::write(path, text).unwrap();
    assert_round2_attribute_siblings(&fixture);
}

#[test]
fn guarded_failure_preserves_cli_lsp_sibling_findings() {
    assert_round2_attribute_siblings(&round2_attribute_fixture("guarded_failure_siblings"));
}
