//! Both clients consume the same report for a directory, including missing
//! nested targets and sibling-file references/expressions.
use super::*;
use carina_core::schema::{
    InputCondition, InputLiteral, InputPath, InputPredicate, InputRule, InputRuleCode,
    InputRuleScope,
};

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
