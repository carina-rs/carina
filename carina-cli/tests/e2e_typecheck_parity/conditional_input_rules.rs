//! Both clients consume the same report for a directory, including missing
//! nested targets and sibling-file references/expressions.
use super::*;
use carina_core::schema::{
    InputCondition, InputLiteral, InputPath, InputPredicate, InputRule, InputRuleCode,
    InputRuleScope,
};

#[test]
fn missing_intermediate_objects_have_cli_lsp_directory_parity_at_the_nearest_parent() {
    use carina_core::schema::StructField;
    let schema = ResourceSchema::new("test.Conditional")
        .attribute(
            AttributeSchema::optional(
                "config",
                AttributeType::struct_(
                    "Config",
                    vec![
                        StructField::optional(
                            "outer",
                            AttributeType::struct_(
                                "Outer",
                                vec![
                                    StructField::optional(
                                        "inner",
                                        AttributeType::struct_(
                                            "Inner",
                                            vec![
                                                StructField::optional(
                                                    "target",
                                                    AttributeType::bool(),
                                                )
                                                .build(),
                                            ],
                                        ),
                                    )
                                    .build(),
                                ],
                            ),
                        )
                        .build(),
                    ],
                ),
            )
            .build(),
        )
        .input_rule(InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("config.outer.inner.target").unwrap(),
            InputRuleCode::new("test.inner_target").unwrap(),
            InputPredicate::Present,
        ));
    let source = "let example = test.test.Conditional {\n  config = {\n    outer = {}\n  }\n}\n";
    let fixture = write_fixture(&[
        ("main.crn", source),
        ("providers.crn", "provider test {}\n"),
    ]);
    let schemas = single_schema_map(schema);
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let cli = cli
        .iter()
        .flat_map(|message| message.lines())
        .find(|message| message.contains("[test.inner_target]"))
        .expect("CLI input finding");
    let diagnostics = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let findings = diagnostics
        .iter()
        .filter(|finding| finding.message == cli)
        .collect::<Vec<_>>();
    assert_eq!(findings.len(), 1, "{diagnostics:#?}");
    assert_eq!(findings[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        findings[0].range.start.line, 2,
        "nearest authored object is outer, not config"
    );
    assert_eq!(findings[0].range.start.character, 12);
}

#[test]
fn alias_conflicts_preserve_independent_cli_lsp_create_only_warnings() {
    let schema = ResourceSchema::new("test.Conditional")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional(
                "entries",
                AttributeType::list(AttributeType::struct_("Entry", vec![])),
            )
            .with_block_name("entry")
            .build(),
        )
        .input_rule(InputRule::value(
            InputRuleScope::CreateOnly,
            InputPath::new("name").unwrap(),
            InputRuleCode::new("test.creation_name").unwrap(),
            InputPredicate::NoSuffix(carina_core::schema::NonEmptyString::new("-an").unwrap()),
        ));
    let fixture = write_fixture(&[
        ("providers.crn", "provider test {}\n"),
        (
            "main.crn",
            "let example = test.test.Conditional { name = 'bucket-an' }\n",
        ),
        (
            "broken.crn",
            "let broken = test.test.Conditional { entries = [] entry {} }\n",
        ),
    ]);
    let schemas = single_schema_map(schema);
    let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories_for(&schemas)),
        Default::default(),
    );
    assert!(!cli.errors.is_empty(), "alias conflict remains an error");
    assert_eq!(
        cli.input_warnings.len(),
        1,
        "independent warning must survive: {:?}",
        cli.errors
    );
    let message = cli.input_warnings[0].to_string();
    assert!(message.ends_with("(rejected if this resource is created)"));
    let engine = engine_with_schemas(schemas);
    let diagnostics = lsp_diagnostics(&engine, &fixture, "main.crn");
    let warnings = diagnostics
        .iter()
        .filter(|finding| finding.message == message)
        .collect::<Vec<_>>();
    assert_eq!(warnings.len(), 1, "{diagnostics:#?}");
    assert_eq!(warnings[0].severity, Some(DiagnosticSeverity::WARNING));
    let broken = lsp_diagnostics(&engine, &fixture, "broken.crn");
    assert!(
        broken.iter().any(
            |finding| finding.severity == Some(DiagnosticSeverity::ERROR)
                && finding.message.contains("cannot use both")
        ),
        "{broken:#?}"
    );
}

#[test]
fn invalid_module_default_tags_preserve_independent_cli_lsp_input_findings() {
    let schema = ResourceSchema::new("test.Conditional")
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("fallback", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("marker", AttributeType::string()).build())
        .attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        )
        .input_rule(InputRule::value(
            InputRuleScope::CreateOnly,
            InputPath::new("name").unwrap(),
            InputRuleCode::new("test.creation_name").unwrap(),
            InputPredicate::NoSuffix(carina_core::schema::NonEmptyString::new("-an").unwrap()),
        ))
        .input_rule(InputRule::value(
            InputRuleScope::EveryWrite,
            InputPath::new("target").unwrap(),
            InputRuleCode::new("test.required_target").unwrap(),
            InputPredicate::Present,
        ))
        .input_rule(
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("marker").unwrap(),
                InputRuleCode::new("test.tagged_marker").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(InputCondition::Present(InputPath::new("tags").unwrap())),
        )
        .input_rule(
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("fallback").unwrap(),
                InputRuleCode::new("test.absent_tags_fallback").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(InputCondition::Not(Box::new(InputCondition::Present(
                InputPath::new("tags").unwrap(),
            )))),
        );
    let fixture = write_fixture(&[
        (
            "providers.crn",
            "provider test { default_tags = m.tags }\nprovider later { default_tags = m.good_tags }\n",
        ),
        (
            "main.crn",
            "let module = use { source = './module' }\nlet m = module {}\nlet example = test.test.Conditional { name = 'bucket-an' }\nlet after = later.test.Conditional { name = 'valid' target = 'provided' }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("module")).unwrap();
    std::fs::write(
        fixture.path().join("module/main.crn"),
        "attributes { tags = 'not-a-map' good_tags = { owner = 'known' } }\n",
    )
    .unwrap();
    let mut schemas = single_schema_map(schema.clone());
    schemas.insert("later", schema.validate().unwrap());
    let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories_for(&schemas)),
        Default::default(),
    );
    let errors = cli
        .errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(
        errors.iter().any(|error| error.contains("default_tags")),
        "{errors:#?}"
    );
    assert_eq!(cli.input_warnings.len(), 1, "{errors:#?}");
    let target = errors
        .iter()
        .flat_map(|error| error.lines())
        .find(|error| error.contains("[test.required_target]"))
        .expect("independent EveryWrite finding");
    let markers = errors
        .iter()
        .flat_map(|error| error.lines())
        .filter(|error| error.contains("[test.tagged_marker]"))
        .collect::<Vec<_>>();
    assert_eq!(markers.len(), 1, "{errors:#?}");
    assert!(markers[0].contains("later.test.Conditional.after"));
    let diagnostics = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    for (message, severity) in [
        (target.to_owned(), DiagnosticSeverity::ERROR),
        (markers[0].to_owned(), DiagnosticSeverity::ERROR),
        (
            cli.input_warnings[0].to_string(),
            DiagnosticSeverity::WARNING,
        ),
    ] {
        let findings = diagnostics
            .iter()
            .filter(|finding| finding.message == message)
            .collect::<Vec<_>>();
        assert_eq!(findings.len(), 1, "{diagnostics:#?}");
        assert_eq!(findings[0].severity, Some(severity));
    }
    assert!(
        !errors
            .iter()
            .any(|error| error.contains("[test.absent_tags_fallback]")),
        "{errors:#?}"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|finding| finding.message.contains("[test.absent_tags_fallback]")),
        "{diagnostics:#?}"
    );
}

#[test]
fn module_default_tags_have_cli_lsp_conditional_rule_parity() {
    let schema = ResourceSchema::new("test.Conditional")
        .attribute(
            AttributeSchema::optional("tags", AttributeType::map(AttributeType::string())).build(),
        )
        .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
        .input_rule(
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("target").unwrap(),
                InputRuleCode::new("test.tagged_target").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(InputCondition::Present(InputPath::new("tags").unwrap())),
        );
    let fixture = write_fixture(&[
        (
            "main.crn",
            "let tag_module = use { source = './tag-module' }\nlet tags = tag_module {}\nlet example = test.test.Conditional {}\n",
        ),
        (
            "providers.crn",
            "provider test { default_tags = tags.tags }\n",
        ),
    ]);
    std::fs::create_dir(fixture.path().join("tag-module")).unwrap();
    std::fs::write(
        fixture.path().join("tag-module/main.crn"),
        "attributes { tags = { owner = 'known' } }\n",
    )
    .unwrap();
    let schemas = single_schema_map(schema);
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let cli_findings = cli
        .iter()
        .flat_map(|message| message.lines())
        .filter(|message| message.contains("[test.tagged_target]"))
        .collect::<Vec<_>>();
    assert_eq!(cli_findings.len(), 1, "{cli:#?}");
    let diagnostics = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let lsp_findings = diagnostics
        .iter()
        .filter(|finding| finding.message.contains("[test.tagged_target]"))
        .collect::<Vec<_>>();
    assert_eq!(lsp_findings.len(), 1, "{diagnostics:#?}");
    assert_eq!(cli_findings[0], lsp_findings[0].message);
    assert_eq!(lsp_findings[0].severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn unrelated_type_errors_preserve_cli_lsp_input_errors_and_warnings() {
    let schema = ResourceSchema::new("test.Conditional")
        .attribute(AttributeSchema::optional("count", AttributeType::int()).build())
        .attribute(AttributeSchema::optional("guard", AttributeType::bool()).build())
        .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
        .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
        .input_rule(InputRule::value(
            InputRuleScope::CreateOnly,
            InputPath::new("name").unwrap(),
            InputRuleCode::new("test.creation_name").unwrap(),
            InputPredicate::NoSuffix(carina_core::schema::NonEmptyString::new("-an").unwrap()),
        ))
        .input_rule(
            InputRule::value(
                InputRuleScope::EveryWrite,
                InputPath::new("target").unwrap(),
                InputRuleCode::new("test.required_target").unwrap(),
                InputPredicate::Present,
            )
            .with_condition(InputCondition::Equals {
                path: InputPath::new("guard").unwrap(),
                literal: InputLiteral::Bool(true),
            }),
        );
    let fixture = write_fixture(&[
        (
            "main.crn",
            "let example = test.test.Conditional { name = 'bucket-an' guard = true }\n",
        ),
        ("providers.crn", "provider test {}\n"),
        (
            "broken.crn",
            "let broken = test.test.Conditional { count = 'not-a-number' }\n",
        ),
    ]);
    let schemas = single_schema_map(schema);
    let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
        &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
        carina_cli::wiring::LoadedProviderFactory::for_test(factories_for(&schemas)),
        Default::default(),
    );
    let cli_errors = cli
        .errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    assert!(
        cli_errors.iter().any(|message| message.contains("Int")),
        "{cli_errors:?}"
    );
    assert_eq!(cli.input_warnings.len(), 1, "{cli_errors:?}");
    let errors = cli_errors
        .iter()
        .flat_map(|message| message.lines())
        .filter(|message| message.contains("[test.required_target]"))
        .collect::<Vec<_>>();
    assert_eq!(errors.len(), 1, "{cli_errors:?}");
    let engine = engine_with_schemas(schemas);
    let diagnostics = lsp_diagnostics(&engine, &fixture, "main.crn");
    for (message, severity) in [
        (errors[0].to_owned(), DiagnosticSeverity::ERROR),
        (
            cli.input_warnings[0].to_string(),
            DiagnosticSeverity::WARNING,
        ),
    ] {
        let findings = diagnostics
            .iter()
            .filter(|finding| finding.message == message)
            .collect::<Vec<_>>();
        assert_eq!(findings.len(), 1, "{diagnostics:#?}");
        assert_eq!(findings[0].severity, Some(severity));
    }
    let broken = lsp_diagnostics(&engine, &fixture, "broken.crn");
    assert!(
        broken.iter().any(|finding| finding.message.contains("Int")),
        "{broken:#?}"
    );
}

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
