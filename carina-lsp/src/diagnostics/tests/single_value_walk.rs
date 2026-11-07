use super::*;
use carina_core::provider::{
    BoxFuture, CanonicalCustomTypeValue, CanonicalProviderConfigAttributes, InitializeOutcomeMint,
    ProviderError, ProviderInitialization, ProviderReadyConfig, ProviderResult,
};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, StructField, TypeIdentity,
    ValidatedConfigSchema, ValidatedProviderSchemaWitness, ValidatedResourceSchema,
};
use tower_lsp::lsp_types::{Position, Range};

struct WalkFactory(ValidatedResourceSchema);

impl ProviderFactory for WalkFactory {
    fn name(&self) -> &str {
        "test"
    }
    fn display_name(&self) -> &str {
        "test"
    }
    fn schemas(&self) -> Vec<ValidatedResourceSchema> {
        vec![self.0.clone()]
    }
    fn provider_config_attribute_types(&self) -> ValidatedConfigSchema {
        Default::default()
    }
    fn schema_witness(&self) -> ValidatedProviderSchemaWitness {
        ValidatedProviderSchemaWitness::new(
            self.schemas(),
            Default::default(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }
    fn validate_config(&self, _: &CanonicalProviderConfigAttributes) -> ProviderResult<()> {
        Ok(())
    }
    fn validate_custom_type(
        &self,
        identity: &TypeIdentity,
        value: &CanonicalCustomTypeValue,
    ) -> ProviderResult<()> {
        if (identity.kind == "SubnetId" && value.as_str().starts_with("subnet-"))
            || (identity.kind == "VpcId" && value.as_str().starts_with("vpc-"))
        {
            return Ok(());
        }
        Err(ProviderError::invalid_input(format!(
            "{} rejects {}",
            identity.kind,
            value.as_str()
        )))
    }
    fn create_provider(
        &self,
        _: Option<&str>,
        _: &ProviderReadyConfig,
        _: InitializeOutcomeMint,
    ) -> BoxFuture<'_, ProviderResult<ProviderInitialization>> {
        Box::pin(async { Err(ProviderError::internal("unused test factory")) })
    }
}

fn engine(ty: AttributeType) -> DiagnosticEngine {
    engine_with_schema(
        ResourceSchema::new("test.Widget")
            .attribute(AttributeSchema::optional("value", ty).build()),
    )
}

fn engine_with_schema(schema: ResourceSchema) -> DiagnosticEngine {
    DiagnosticEngine::new(
        loaded_providers_from_factories(vec![Box::new(WalkFactory(schema.validate().unwrap()))]),
        vec!["test".into()],
    )
}

fn remote(name: &str) -> AttributeType {
    AttributeType::refined_string(Some(TypeIdentity::bare(name)), None, None, None)
}

fn value_diagnostics(engine: &DiagnosticEngine, source: &str) -> Vec<Diagnostic> {
    engine
        .analyze(&create_document(source), None)
        .into_iter()
        .filter(|diagnostic| {
            matches!(
                diagnostic.severity,
                Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)
            )
        })
        .collect()
}

#[test]
fn issue_4276_round2_a_local_union_retains_only_first_tied_provider_findings() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::struct_(
            "First",
            vec![
                StructField::required("v", remote("VpcId")).build(),
                StructField::required("x", AttributeType::int()).build(),
            ],
        ),
        AttributeType::struct_(
            "Second",
            vec![
                StructField::required("v", remote("SubnetId")).build(),
                StructField::required("y", AttributeType::int()).build(),
            ],
        ),
    ]));
    let diagnostics = value_diagnostics(
        &engine,
        "test.test.Widget {\n  value = { v = \"bad\" }\n}\n",
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Expected one of these forms: ('v' and 'x') or ('v' and 'y')",
            "test rejected `VpcId`: VpcId rejects bad",
        ]
    );
}

#[test]
fn issue_4276_round2_b_provider_only_union_uses_fewest_errors() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::map(remote("SubnetId")),
        AttributeType::struct_(
            "Named",
            vec![
                StructField::required("a", remote("VpcId")).build(),
                StructField::required("b", remote("VpcId")).build(),
            ],
        ),
    ]));
    let diagnostics = value_diagnostics(
        &engine,
        "test.test.Widget {\n  value = { a = \"bad\", b = \"subnet-1\" }\n}\n",
    );
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec!["test rejected `SubnetId`: SubnetId rejects bad",]
    );
}

#[test]
fn issue_4276_round2_c_identified_module_if_reports_local_branch_error() {
    let engine = engine(AttributeType::refined_string(
        Some(TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(5))),
        None,
    ));
    let source = "arguments { flag: Bool }\ntest.test.Widget {\n  value = if flag { \"vpc-1\" } else { \"vpc-123\" }\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(
        diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect::<Vec<_>>(),
        vec![
            "Invalid value 'vpc-123' for 'value' (VpcId): length 7 is outside allowed range [, 5]",
        ]
    );
}

#[test]
fn issue_4276_round2_c_module_if_reports_errors_from_both_branches() {
    let engine = engine(AttributeType::refined_string(
        Some(TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(5))),
        None,
    ));
    for both_local in [false, true] {
        let first = if both_local { "bad-long" } else { "bad" };
        let source = format!(
            "arguments {{ flag: Bool }}\ntest.test.Widget {{\n  value = if flag {{ \"{first}\" }} else {{ \"vpc-123\" }}\n}}\n"
        );
        let diagnostics = value_diagnostics(&engine, &source);
        let mut expected = vec![
            "Invalid value 'vpc-123' for 'value' (VpcId): length 7 is outside allowed range [, 5]",
        ];
        if both_local {
            expected.insert(0, "Invalid value 'bad-long' for 'value' (VpcId): length 8 is outside allowed range [, 5]");
        } else {
            expected.push("test rejected `VpcId`: VpcId rejects bad");
        }
        assert_eq!(
            diagnostics
                .iter()
                .map(|d| d.message.as_str())
                .collect::<Vec<_>>(),
            expected
        );
    }
}

#[test]
fn issue_4276_function_container_uses_authored_findings() {
    let engine = engine(AttributeType::struct_(
        "Settings",
        vec![StructField::optional("name", AttributeType::string()).build()],
    ));
    let source = "let broken = test.test.Widget { value = length(1) }\ntest.test.Widget {\n  value = split(\",\", \"a,b\")\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Invalid expression at line 1: length(): length() argument must be a List, Map, or String, got Int"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 13), Position::new(0, 51))
    );
}

#[test]
fn issue_4276_union_function_uses_authored_findings() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::int(),
        AttributeType::bool(),
    ]));
    let source = "let broken = test.test.Widget { value = length(1) }\ntest.test.Widget {\n  value = split(\",\", \"a,b\")\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Invalid expression at line 1: length(): length() argument must be a List, Map, or String, got Int"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 13), Position::new(0, 51))
    );
}

#[test]
fn issue_4276_coalesce_uses_authored_findings() {
    let engine = engine(AttributeType::int());
    let source = "test.test.Widget {\n  value = length([1]) ?? \"bad\"\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "coalesce default type `String` does not match left type `Int`"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(1, 25), Position::new(1, 30))
    );
    assert_eq!(
        diagnostics[1].message,
        "Type mismatch: expected Int, got String"
    );
    assert_eq!(diagnostics[1].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        diagnostics[1].range,
        Range::new(Position::new(1, 2), Position::new(1, 7))
    );
}

#[test]
fn issue_4276_coalesce_provider_failures_render_once() {
    let engine = engine(remote("Remote"));
    let source = "test.test.Widget {\n  value = secret(\"left\") ?? \"right\"\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let messages = diagnostics
        .iter()
        .map(|d| &d.message)
        .collect::<HashSet<_>>();
    assert_eq!(messages.len(), 1, "{diagnostics:#?}");
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
        assert_eq!(
            diagnostic.message,
            "Type mismatch: expected Remote, got String (from (secret) ?? 'right')"
        );
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(1, 2), Position::new(1, 7))
        );
    }
}

#[test]
fn issue_4276_union_provider_failures_render_once() {
    let engine = engine(AttributeType::union(vec![
        remote("First"),
        remote("Second"),
    ]));
    let source = "test.test.Widget {\n  value = secret(\"left\") ?? \"right\"\n}\n";
    let diagnostics = value_diagnostics(&engine, source);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].message,
        "Type mismatch: expected First | Second, got String (from (secret) ?? 'right')"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(1, 2), Position::new(1, 7))
    );
}

fn directory_diagnostics(
    engine: &DiagnosticEngine,
    files: &[(&str, &str)],
    filename: &str,
) -> Vec<Diagnostic> {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider test {}\n").unwrap();
    for (filename, source) in files {
        let path = directory.path().join(filename);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }
    let path = directory.path().join(filename);
    let source = std::fs::read_to_string(&path).unwrap();
    engine.analyze(&create_document_at(&source, path.clone()), Some(&path))
}

#[test]
fn input_rules_produce_no_lsp_value_findings() {
    use carina_core::schema::{InputPath, InputPredicate, InputRule, InputRuleCode};
    let engine = engine_with_schema(
        ResourceSchema::new("test.Widget")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .input_rule(InputRule::value(
                carina_core::schema::InputRuleScope::CreateOnly,
                InputPath::new("name").unwrap(),
                InputRuleCode::new("test.not_ipv4").unwrap(),
                InputPredicate::NotIpv4,
            )),
    );
    let diagnostics = directory_diagnostics(
        &engine,
        &[
            ("values.crn", "let legacy = '192.168.1.1'\n"),
            ("exports.crn", "exports { name = bucket.name }\n"),
            (
                "main.crn",
                "let bucket = test.test.Widget { name = legacy }\n",
            ),
        ],
        "main.crn",
    );
    assert!(diagnostics.is_empty(), "{diagnostics:?}");
}

#[test]
fn input_every_write_matches_validate_for_multifile_desired_values_and_defaults() {
    use carina_core::schema::{
        InputPath, InputPredicate, InputRule, InputRuleCode, InputRuleScope, InputRuleTarget,
        NonEmptyString,
    };
    for scope in [InputRuleScope::CreateOnly, InputRuleScope::EveryWrite] {
        let engine = engine_with_schema(
            ResourceSchema::new("test.Widget")
                .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::optional("tags", AttributeType::map(AttributeType::string()))
                        .build(),
                )
                .input_rule(InputRule::new(
                    scope,
                    InputRuleTarget::MapKeys(InputPath::new("tags").unwrap()),
                    InputRuleCode::new("tags.reserved_prefix").unwrap(),
                    InputPredicate::ForbiddenPrefix {
                        prefix: NonEmptyString::new("aws:").unwrap(),
                        ignore_ascii_case: true,
                    },
                )),
        );
        for via_default in [false, true] {
            let providers = if via_default {
                "provider test { default_tags = { 'AWS:private-key' = 'secret-value' } }\n"
            } else {
                "provider test {}\n"
            };
            let main = if via_default {
                "let bucket = test.test.Widget { name = 'valid' }\n"
            } else {
                "let bucket = test.test.Widget { name = 'valid' tags = shared_tags }\n"
            };
            let diagnostics = directory_diagnostics(
                &engine,
                &[
                    ("providers.crn", providers),
                    (
                        "values.crn",
                        "let shared_tags = { 'aWs:private-key' = 'secret-value' }\n",
                    ),
                    ("main.crn", main),
                ],
                "main.crn",
            );
            let findings = diagnostics
                .iter()
                .filter(|d| d.message.contains("tags.reserved_prefix"))
                .collect::<Vec<_>>();
            assert_eq!(
                findings.len(),
                usize::from(scope == InputRuleScope::EveryWrite),
                "{scope:?}, default={via_default}: {diagnostics:#?}"
            );
            for finding in findings {
                assert_eq!(finding.severity, Some(DiagnosticSeverity::ERROR));
                assert!(finding.message.contains("has a forbidden prefix"));
                assert!(!finding.message.contains("private-key"));
                assert!(!finding.message.contains("secret-value"));
            }
        }
    }
}

fn assert_directory_findings(
    diagnostics: Vec<Diagnostic>,
    expected: &[(&str, DiagnosticSeverity, Range)],
) {
    let actual = diagnostics
        .iter()
        .filter_map(|diagnostic| match diagnostic.severity {
            Some(severity @ (DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)) => {
                Some((diagnostic.message.as_str(), severity, diagnostic.range))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(actual, expected, "{diagnostics:#?}");
}

fn range(line: u32, start: u32, end: u32) -> Range {
    Range::new(Position::new(line, start), Position::new(line, end))
}

#[test]
fn directory_f03_reports_only_the_local_length_error() {
    let engine = engine(AttributeType::refined_string(
        Some(TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(3))),
        None,
    ));
    let source = "test.test.Widget {\n  value = \"abcd\"\n}\n";

    assert_directory_findings(
        directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
        &[(
            "Invalid value 'abcd' for 'value' (VpcId): length 4 is outside allowed range [, 3]",
            DiagnosticSeverity::ERROR,
            range(1, 2, 7),
        )],
    );
}

#[test]
fn directory_m01_union_keeps_the_provider_rejection() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::int(),
        remote("VpcId"),
    ]));
    let source = "test.test.Widget {\n  value = \"bad\"\n}\n";

    assert_directory_findings(
        directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
        &[(
            "test rejected `VpcId`: VpcId rejects bad",
            DiagnosticSeverity::ERROR,
            range(1, 2, 7),
        )],
    );
}

#[test]
fn directory_nested_union_keeps_the_inner_provider_rejection() {
    let engine = engine_with_schema(
        ResourceSchema::new("test.Widget")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::union(vec![AttributeType::ref_("Inner"), AttributeType::bool()]),
                )
                .build(),
            )
            .with_def(
                "Inner",
                AttributeType::union(vec![
                    AttributeType::map(remote("VpcId")),
                    AttributeType::struct_(
                        "Named",
                        vec![StructField::required("name", AttributeType::string()).build()],
                    ),
                ]),
            ),
    );
    let source = "test.test.Widget {\n  value = { foo = \"bad\" }\n}\n";

    assert_directory_findings(
        directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
        &[(
            "test rejected `VpcId`: VpcId rejects bad",
            DiagnosticSeverity::ERROR,
            range(1, 2, 7),
        )],
    );
}

#[test]
fn directory_union_tie_reports_the_provider_rejection_once() {
    let engine = engine(AttributeType::union(vec![
        remote("VpcId"),
        AttributeType::refined_string(
            None,
            Some(carina_core::schema::PatternSpec::new(
                "^x",
                carina_core::schema::PatternMatch::Full,
            )),
            None,
            None,
        ),
    ]));
    let source = "test.test.Widget {\n  value = \"bad\"\n}\n";

    assert_directory_findings(
        directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
        &[(
            "test rejected `VpcId`: VpcId rejects bad",
            DiagnosticSeverity::ERROR,
            range(1, 2, 7),
        )],
    );
}

#[test]
fn directory_local_union_tie_keeps_only_the_first_arms_provider_findings() {
    let engine = engine(AttributeType::union(vec![
        AttributeType::struct_(
            "First",
            vec![
                StructField::required("v", remote("VpcId")).build(),
                StructField::required("x", AttributeType::int()).build(),
            ],
        ),
        AttributeType::struct_(
            "Second",
            vec![
                StructField::required("v", remote("SubnetId")).build(),
                StructField::required("y", AttributeType::int()).build(),
            ],
        ),
    ]));
    let source = "test.test.Widget {\n  value = { v = \"bad\" }\n}\n";

    assert_directory_findings(
        directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
        &[
            (
                "Expected one of these forms: ('v' and 'x') or ('v' and 'y')",
                DiagnosticSeverity::WARNING,
                range(1, 2, 7),
            ),
            (
                "test rejected `VpcId`: VpcId rejects bad",
                DiagnosticSeverity::ERROR,
                range(1, 2, 7),
            ),
        ],
    );
}

#[test]
fn directory_unknown_struct_keys_keep_their_written_positions() {
    let engine = engine(AttributeType::struct_(
        "Versioning",
        vec![StructField::optional("status", AttributeType::string()).build()],
    ));
    for second_key in [false, true] {
        let source = if second_key {
            "test.test.Widget {\n  value = {\n    statuus = \"Enabled\"\n    mfa = false\n  }\n}\n"
        } else {
            "test.test.Widget {\n  value = {\n    statuus = \"Enabled\"\n  }\n}\n"
        };
        let mut expected = vec![(
            "Unknown field 'statuus' in Versioning, did you mean 'status'?",
            DiagnosticSeverity::WARNING,
            range(2, 4, 11),
        )];
        if second_key {
            expected.push((
                "Unknown field 'mfa' in Versioning",
                DiagnosticSeverity::WARNING,
                range(3, 4, 7),
            ));
        }

        assert_directory_findings(
            directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
            &expected,
        );
    }
}

#[test]
fn directory_unknown_struct_key_is_retained_with_a_missing_required_field() {
    let engine = engine(AttributeType::struct_(
        "Versioning",
        vec![StructField::required("status", AttributeType::string()).build()],
    ));
    let source = "test.test.Widget {\n  value = {\n    statuus = \"Enabled\"\n  }\n}\n";

    assert_directory_findings(
        directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
        &[
            (
                "Required attribute 'status' is missing",
                DiagnosticSeverity::WARNING,
                range(1, 2, 7),
            ),
            (
                "Unknown field 'statuus' in Versioning, did you mean 'status'?",
                DiagnosticSeverity::WARNING,
                range(2, 4, 11),
            ),
            (
                "Required attribute 'status' is missing",
                DiagnosticSeverity::ERROR,
                range(0, 0, 16),
            ),
        ],
    );
}

#[test]
fn directory_nested_read_only_struct_key_keeps_its_written_position() {
    let engine = engine(AttributeType::struct_(
        "Config",
        vec![
            StructField::optional(
                "destination",
                AttributeType::struct_(
                    "Destination",
                    vec![StructField::provider_populated("arn", AttributeType::string()).build()],
                ),
            )
            .build(),
        ],
    ));
    let source = "test.test.Widget {\n  value = {\n    destination = {\n      arn = \"provider-arn\"\n    }\n  }\n}\n";

    assert_directory_findings(
        directory_diagnostics(&engine, &[("main.crn", source)], "main.crn"),
        &[(
            "Attribute 'arn' is populated by the provider and cannot be set",
            DiagnosticSeverity::ERROR,
            range(3, 6, 9),
        )],
    );
}

#[test]
fn directory_child_module_if_checks_each_undecided_branch() {
    let engine = engine(AttributeType::refined_string(
        Some(TypeIdentity::bare("VpcId")),
        None,
        Some((None, Some(5))),
        None,
    ));
    for first in ["vpc-1", "bad", "bad-long"] {
        let source = format!(
            "test.test.Widget {{\n  value = if flag {{ \"{first}\" }} else {{ \"vpc-123\" }}\n}}\n"
        );
        let diagnostics = directory_diagnostics(
            &engine,
            &[
                (
                    "main.crn",
                    "let child = use { source = './child' }\nchild { flag = true }\n",
                ),
                ("child/arguments.crn", "arguments { flag: Bool }\n"),
                ("child/main.crn", &source),
            ],
            "child/main.crn",
        );
        let mut expected = vec![(
            "Invalid value 'vpc-123' for 'value' (VpcId): length 7 is outside allowed range [, 5]",
            DiagnosticSeverity::ERROR,
            range(1, 2, 7),
        )];
        if first == "bad-long" {
            expected.insert(0, (
                "Invalid value 'bad-long' for 'value' (VpcId): length 8 is outside allowed range [, 5]",
                DiagnosticSeverity::ERROR,
                range(1, 2, 7),
            ));
        } else if first == "bad" {
            expected.push((
                "test rejected `VpcId`: VpcId rejects bad",
                DiagnosticSeverity::ERROR,
                range(1, 2, 7),
            ));
        }
        assert_directory_findings(diagnostics, &expected);
    }
}
