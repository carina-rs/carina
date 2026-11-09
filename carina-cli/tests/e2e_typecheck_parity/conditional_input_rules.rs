//! Both clients consume the same report for a directory, including missing
//! nested targets and sibling-file references/expressions.
use super::*;
use carina_core::schema::{
    InputCondition, InputLiteral, InputPath, InputPredicate, InputRule, InputRuleCode,
    InputRuleScope,
};

#[test]
fn renamed_blocks_have_multifile_cli_lsp_conditional_rule_parity() {
    let schema = ResourceSchema::new("test.Conditional").attribute(
        AttributeSchema::optional(
            "entries",
            AttributeType::list(AttributeType::struct_with_rules(
                "Entry",
                vec![
                    StructField::optional("guard", AttributeType::bool()).build(),
                    StructField::optional("target", AttributeType::string()).build(),
                ],
                vec![],
                vec![
                    InputRule::value(
                        InputRuleScope::EveryWrite,
                        InputPath::new("target").unwrap(),
                        InputRuleCode::new("test.entry_target").unwrap(),
                        InputPredicate::Present,
                    )
                    .with_condition(InputCondition::Equals {
                        path: InputPath::new("guard").unwrap(),
                        literal: InputLiteral::Bool(true),
                    }),
                ],
            )),
        )
        .with_block_name("entry")
        .build(),
    );
    let fixture = write_fixture(&[
        (
            "main.crn",
            "let example = test.test.Conditional {\n  entry {\n    guard = trigger\n  }\n}\n",
        ),
        ("providers.crn", "provider test {}\nlet trigger = true\n"),
    ]);
    let schemas = single_schema_map(schema);
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let cli_findings = cli
        .iter()
        .flat_map(|message| message.lines())
        .filter(|message| message.contains("[test.entry_target]"))
        .collect::<Vec<_>>();
    assert_eq!(cli_findings.len(), 1, "{cli:#?}");
    let lsp_findings = lsp
        .iter()
        .filter(|d| d.message.contains("[test.entry_target]"))
        .collect::<Vec<_>>();
    assert_eq!(lsp_findings.len(), 1, "{lsp:#?}");
    assert_eq!(cli_findings[0], lsp_findings[0].message);
    assert!(lsp_findings[0].message.contains("entries[0].target"));
    assert_eq!(lsp_findings[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        lsp_findings[0].range.start.line, 1,
        "anchor at the authored entry block"
    );
}

#[test]
fn create_only_s3_rules_have_multifile_cli_lsp_warning_and_pending_parity() {
    let identity =
        carina_core::schema::TypeIdentity::new(Some("test"), ["s3", "Bucket"], "Namespace");
    let namespace_type = AttributeType::enum_(
        identity.clone(),
        Some(vec!["global".into(), "account-regional".into()]),
        vec![
            ("global".into(), "global".into()),
            ("account-regional".into(), "account_regional".into()),
        ],
        None,
        None,
    );
    let schema = ResourceSchema::new("s3.Bucket")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("bucket_namespace", namespace_type.clone()).build())
        .input_rule(
            InputRule::value(
                InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("s3.bucket.namespace_suffix").unwrap(),
                InputPredicate::NoSuffix(carina_core::schema::NonEmptyString::new("-an").unwrap()),
            )
            .with_condition(InputCondition::Not(Box::new(InputCondition::Equals {
                path: InputPath::new("bucket_namespace").unwrap(),
                literal: InputLiteral::Enum {
                    identity,
                    api_value: "account-regional".into(),
                },
            }))),
        );
    for (namespace, expected_count) in [
        ("", 1),
        (
            "bucket_namespace = test.s3.Bucket.Namespace.account_regional",
            0,
        ),
        ("bucket_namespace = namespace.value", 0),
    ] {
        let source = format!("let bucket = test.s3.Bucket {{ name = bucket_name {namespace} }}\n");
        let fixture = write_fixture(&[
            ("main.crn", &source),
            (
                "providers.crn",
                "provider test {}\nlet bucket_name = 'bucket-an'\nlet namespace = test.s3.Namespace {}\n",
            ),
        ]);
        let mut schemas = single_schema_map(schema.clone());
        schemas.insert(
            "test",
            ResourceSchema::new("s3.Namespace")
                .attribute(
                    AttributeSchema::provider_populated("value", namespace_type.clone()).build(),
                )
                .validate()
                .unwrap(),
        );
        let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories_for(&schemas)),
            Default::default(),
        );
        assert!(cli.errors.is_empty(), "{namespace}: {:?}", cli.errors);
        assert_eq!(cli.input_warnings.len(), expected_count);
        let diagnostics = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        let lsp = diagnostics
            .iter()
            .filter(|d| d.message.contains("[s3.bucket.namespace_suffix]"))
            .collect::<Vec<_>>();
        assert_eq!(lsp.len(), expected_count, "{namespace}: {diagnostics:#?}");
        for (warning, diagnostic) in cli.input_warnings.iter().zip(lsp) {
            assert_eq!(
                warning.severity(),
                carina_core::input_rules::InputFindingSeverity::Warning
            );
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
            assert_eq!(warning.to_string(), diagnostic.message);
            assert_eq!(
                diagnostic.message,
                "test.s3.Bucket.bucket: name [s3.bucket.namespace_suffix]: has a forbidden suffix when not (bucket_namespace equals test.s3.Bucket.Namespace::\"account-regional\") (rejected if this resource is created)"
            );
        }
    }
}

#[test]
fn every_write_conditional_rules_have_multifile_cli_lsp_message_and_path_parity() {
    let declaration = |target: &str, code: &str, condition| {
        InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new(target).unwrap(),
            InputRuleCode::new(code).unwrap(),
            InputPredicate::Present,
        )
        .with_condition(condition)
    };
    let schema = ResourceSchema::new("test.Conditional")
        .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
        .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional(
                "settings",
                AttributeType::list(AttributeType::struct_with_rules(
                    "Pitr",
                    vec![
                        StructField::optional("period", AttributeType::int()).build(),
                        StructField::optional("enabled", AttributeType::bool()).build(),
                    ],
                    vec![],
                    vec![declaration(
                        "enabled",
                        "test.pitr",
                        InputCondition::Present(InputPath::new("period").unwrap()),
                    )],
                )),
            )
            .build(),
        )
        .input_rule(declaration(
            "target",
            "test.guard",
            InputCondition::Equals {
                path: InputPath::new("guard").unwrap(),
                literal: InputLiteral::Bool(false),
            },
        ));
    let mut previous = None;
    for expression in ["false", "trigger", "if true { trigger } else { true }"] {
        let source = format!(
            "let example = test.test.Conditional {{\n  guard = {expression}\n  settings = [\n    {{ period = 7 enabled = false }},\n    {{ period = 14 }}\n  ]\n}}\n"
        );
        let fixture = write_fixture(&[
            ("main.crn", &source),
            ("providers.crn", "provider test {}\nlet trigger = false\n"),
        ]);
        let schemas = single_schema_map(schema.clone());
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let diagnostics = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        let cli_findings = cli
            .iter()
            .flat_map(|message| message.lines())
            .filter(|message| message.contains("[test.guard]") || message.contains("[test.pitr]"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let lsp_findings = diagnostics
            .iter()
            .filter(|d| d.message.contains("[test.guard]") || d.message.contains("[test.pitr]"))
            .collect::<Vec<_>>();
        assert_eq!(cli_findings.len(), 2, "{expression}: {cli:#?}");
        assert_eq!(lsp_findings.len(), 2, "{expression}: {diagnostics:#?}");
        let messages = message_multiset(lsp_findings.iter().map(|d| d.message.as_str()));
        assert_eq!(
            message_multiset(cli_findings.iter().map(String::as_str)),
            messages
        );
        if let Some(previous) = &previous {
            assert_eq!(&messages, previous);
        }
        previous = Some(messages);
        let nested = lsp_findings
            .iter()
            .find(|d| d.message.contains("[test.pitr]"))
            .unwrap();
        assert!(
            nested.message.contains(
                "settings[1].enabled [test.pitr]: must be present when period is present"
            )
        );
        assert_eq!(
            nested.range.start.line, 4,
            "missing nested target anchors at its existing object"
        );
        assert!(
            lsp_findings
                .iter()
                .all(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        );
    }
}
