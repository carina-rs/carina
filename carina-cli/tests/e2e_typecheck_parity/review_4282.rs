use super::*;

fn schemas(named: bool) -> SchemaRegistry {
    let constrained = || {
        AttributeType::refined_string(
            named.then(|| {
                carina_core::schema::TypeIdentity::from_schema_type("test", "Names", "Name")
            }),
            Some("^[a-z]+$".into()),
            None,
            None,
        )
    };
    single_schema_map(
        ResourceSchema::new("Names")
            .attribute(
                AttributeSchema::optional("names", AttributeType::list(constrained())).build(),
            )
            .attribute(AttributeSchema::optional("word", constrained()).build())
            .attribute(
                AttributeSchema::provider_populated("value", AttributeType::string()).build(),
            ),
    )
}

fn lsp_value_messages(diagnostics: &[tower_lsp::lsp_types::Diagnostic]) -> BTreeMap<String, usize> {
    message_multiset(
        diagnostics
            .iter()
            .filter(|diagnostic| {
                matches!(
                    diagnostic.severity,
                    Some(DiagnosticSeverity::ERROR | DiagnosticSeverity::WARNING)
                )
            })
            .map(|diagnostic| diagnostic.message.as_str()),
    )
}

#[test]
fn issue_4282_sibling_lets_supply_constructed_values_to_cli_and_lsp() {
    let pattern = "does not match required pattern /^[a-z]+$/";
    for sibling in ["a-source.crn", "z-source.crn"] {
        for bound in [false, true] {
            for (expression, input, attribute, invalid, expected_lsp) in [
                (
                    "split(',', text)",
                    "let text = 'a,b'",
                    "names",
                    false,
                    vec![],
                ),
                (
                    "split(',', text)",
                    "let text = 'A,B'",
                    "names",
                    true,
                    vec![
                        format!("Invalid value 'A' for 'names[0]' (Name): {pattern}"),
                        format!("Invalid value 'B' for 'names[1]' (Name): {pattern}"),
                    ],
                ),
                (
                    "join('', parts)",
                    "let parts = ['a', 'b']",
                    "word",
                    false,
                    vec![],
                ),
                (
                    "join('', parts)",
                    "let parts = ['A', 'B']",
                    "word",
                    true,
                    vec![format!("Invalid value 'AB' for 'word' (Name): {pattern}")],
                ),
            ] {
                let (declaration, value) = if bound {
                    (format!("let r = {expression}\n"), "r")
                } else {
                    (String::new(), expression)
                };
                let source = format!("{declaration}test.Names {{ {attribute} = {value} }}\n");
                let fixture = write_fixture(&[
                    ("providers.crn", "provider test {}\n"),
                    ("main.crn", &source),
                    (sibling, input),
                ]);
                let schemas = schemas(true);
                let cli = cli_diagnostics(factories_for(&schemas), &fixture);
                let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
                let expected_cli = if !invalid {
                    vec![]
                } else if attribute == "names" {
                    vec![format!(
                        "test.Names.<pending>: List item at index 0: Invalid value 'A' for 'names' (Name): {pattern}"
                    )]
                } else {
                    vec![format!(
                        "test.Names.<pending>: Invalid value 'AB' for 'word' (Name): {pattern}"
                    )]
                };
                assert_eq!(
                    cli_located_message_multiset(&cli),
                    message_multiset(expected_cli.iter().map(String::as_str)),
                    "{sibling}, bound={bound}, {source}",
                );
                assert_eq!(
                    lsp_value_messages(&lsp),
                    message_multiset(expected_lsp.iter().map(String::as_str)),
                    "{sibling}, bound={bound}, {source}",
                );
            }
        }
    }
}

#[test]
fn issue_4282_sibling_builtin_evaluation_errors_keep_main_messages_and_counts() {
    // These evaluation errors and their counts are unchanged from origin/main.
    // Only the temporary fixture directory is normalized in the CLI message.
    for (expression, sibling, error, unresolved_error) in [
        (
            "split(',', text)",
            "let text = 42",
            "split(): split() second argument must be a string, got Int",
            "Invalid expression at line 1: split(): split() second argument must be a string, got EnumIdentifier",
        ),
        (
            "join('', parts)",
            "let parts = 42",
            "join(): join() second argument must be a list, got Int",
            "Invalid expression at line 1: join(): join() second argument must be a list, got EnumIdentifier",
        ),
    ] {
        let attribute = if expression.starts_with("split(") {
            "names"
        } else {
            "word"
        };
        let main = format!("let r = {expression}\nlet sink = test.Names {{ {attribute} = r }}\n");
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("main.crn", &main),
            ("source.crn", sibling),
        ]);
        let schemas = schemas(false);
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let normalized_cli = cli
            .iter()
            .map(|message| message.replace(fixture.path().to_str().unwrap(), "<fixture>"))
            .collect::<Vec<_>>();
        assert_eq!(
            normalized_cli,
            [format!(
                "<fixture>/main.crn:2:12: Invalid expression at line 1: {error}"
            )],
        );
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        assert_eq!(
            lsp_value_messages(&lsp),
            message_multiset([unresolved_error, error]),
        );
    }
}

#[test]
fn issue_4282_split_coalesce_literal_alternative_keeps_main_diagnostics() {
    for bound in [false, true] {
        let expression = "split(',', x.value) ?? ['UPPER']";
        let source = if bound {
            format!(
                "let x = test.Names {{}}\nlet r = {expression}\nlet sink = test.Names {{ names = r }}\n"
            )
        } else {
            format!("let x = test.Names {{}}\nlet sink = test.Names {{ names = {expression} }}\n")
        };
        let fixture = write_fixture(&[
            ("providers.crn", "provider test {}\n"),
            ("main.crn", &source),
        ]);
        let schemas = schemas(false);
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
        let expected = if bound {
            "List item at index 0: Invalid value 'UPPER': does not match required pattern /^[a-z]+$/"
        } else {
            "List item at index 0: Invalid value 'UPPER' for 'names': does not match required pattern /^[a-z]+$/"
        };
        assert_eq!(
            cli_located_message_multiset(&cli),
            message_multiset(
                [if bound {
                    expected.to_string()
                } else {
                    format!("test.Names.sink: {expected}")
                }]
                .iter()
                .map(String::as_str)
            ),
        );
        assert_eq!(lsp_value_messages(&lsp), message_multiset([expected]));
    }
}
