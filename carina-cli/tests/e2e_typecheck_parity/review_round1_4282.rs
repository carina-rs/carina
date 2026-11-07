use super::*;

fn review_schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("port", AttributeType::int()).build())
            .attribute(AttributeSchema::optional("flag", AttributeType::bool()).build())
            .attribute(
                AttributeSchema::optional("ints", AttributeType::list(AttributeType::int()))
                    .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "names",
                    AttributeType::list(AttributeType::refined_string(
                        Some(carina_core::schema::TypeIdentity::from_schema_type(
                            "mock",
                            "test.Resource",
                            "Name",
                        )),
                        Some(carina_core::schema::PatternSpec::new(
                            "^[a-z]+$",
                            carina_core::schema::PatternMatch::Full,
                        )),
                        None,
                        None,
                    )),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "digit",
                    AttributeType::refined_string(
                        None,
                        Some(carina_core::schema::PatternSpec::new(
                            "^[0-9]$",
                            carina_core::schema::PatternMatch::Full,
                        )),
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .attribute(AttributeSchema::provider_populated("id", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas
}

fn findings(
    source: &str,
    argument_type: Option<&str>,
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        ("main.crn", source),
    ]);
    if let Some(argument_type) = argument_type {
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/arguments.crn"),
            format!("arguments {{ value: {argument_type} }}\n"),
        )
        .unwrap();
        std::fs::write(fixture.path().join("child/main.crn"), "").unwrap();
    }
    let schemas = review_schemas();
    let cli = cli_diagnostics(factories_for(&schemas), &fixture);
    let lsp = lsp_diagnostics(&engine_with_schemas(schemas), &fixture, "main.crn");
    let cli = cli_located_message_multiset(&cli)
        .into_iter()
        .map(|(message, count)| {
            (
                message.replace(fixture.path().to_str().unwrap(), "<fixture>"),
                count,
            )
        })
        .collect();
    (cli, lsp_message_multiset(&lsp))
}

// Compatibility multisets were verified against origin/main 980fff07f.
// Constructed builtin lists use main's equivalent interpolation-list messages.
// Keep its wording and counts even when the two frontends differ.
fn assert_findings(source: &str, argument_type: Option<&str>, cli: Vec<String>, lsp: Vec<String>) {
    let actual = findings(source, argument_type);
    assert_eq!(
        actual.0,
        message_multiset(cli.iter().map(String::as_str)),
        "CLI: {source}"
    );
    assert_eq!(
        actual.1,
        message_multiset(lsp.iter().map(String::as_str)),
        "LSP: {source}"
    );
}

fn pipe_hint(builtin: &str) -> String {
    format!("Consider using pipe form for '{builtin}': data |> {builtin}(...)")
}

#[test]
fn issue_4282_round1_split_non_string_sinks_keep_main_messages_and_counts() {
    for (expression, prefix, deferred) in [
        ("split(\",\", \"a,b\")", "", false),
        ("split(\",\", \"1,2\")", "", false),
        (
            "split(\",\", src.id)",
            "let src = mock.test.Resource {}\n",
            true,
        ),
    ] {
        for (attribute, expected, displayed) in [
            ("name", "String", "String"),
            ("port", "Int", "Int"),
            ("flag", "Bool", "Bool"),
            ("ints", "List<Int>", "list(Int)"),
        ] {
            for bound in [false, true] {
                let (declaration, assigned) = if bound {
                    (format!("let r = {expression}\n"), "r")
                } else {
                    (String::new(), expression)
                };
                let source = format!(
                    "{prefix}{declaration}mock.test.Resource {{ {attribute} = {assigned} }}\n"
                );
                let mut cli = vec![];
                let mut lsp = vec![pipe_hint("split")];
                if bound {
                    let error =
                        format!("Type mismatch: expected {expected}, got List<String> (from r)");
                    cli.push(error.clone());
                    lsp.push(error);
                }
                if deferred {
                    let error = format!("expected {displayed}, got list(String)");
                    cli.push(error.clone());
                    lsp.push(error);
                } else if !bound {
                    if attribute == "ints" {
                        cli.push("mock.test.Resource.<pending>: List item at index 0: Type mismatch: expected Int, got String".into());
                        for index in [0, 1] {
                            lsp.push(format!(
                                "Type mismatch: expected Int, got String for 'ints[{index}]'"
                            ));
                        }
                    } else {
                        cli.push(format!("mock.test.Resource.<pending>: Type mismatch: expected {expected}, got List"));
                    }
                }
                assert_findings(&source, None, cli, lsp);
            }
        }
    }
}

#[test]
fn issue_4282_round1_deferred_builtin_and_interpolation_keep_main_wording() {
    for interpolation in [false, true] {
        let expression = if interpolation {
            r#""${''}${src.id}""#
        } else {
            "join(\"\", [src.id])"
        };
        for (attribute, expected, displayed) in [
            ("name", "String", "String"),
            ("port", "Int", "Int"),
            ("flag", "Bool", "Bool"),
            ("ints", "List<Int>", "list(Int)"),
        ] {
            for bound in [false, true] {
                let (declaration, assigned) = if bound {
                    (format!("let r = {expression}\n"), "r")
                } else {
                    (String::new(), expression)
                };
                let source = format!(
                    "let src = mock.test.Resource {{}}\n{declaration}mock.test.Resource {{ {attribute} = {assigned} }}\n"
                );
                let mut cli = vec![];
                let mut lsp = if interpolation {
                    vec![]
                } else {
                    vec![pipe_hint("join")]
                };
                if attribute != "name" {
                    let error = if interpolation {
                        format!(
                            "cannot assign deferred interpolation: expected {displayed}, got String"
                        )
                    } else {
                        format!("expected {displayed}, got String")
                    };
                    cli.push(error.clone());
                    lsp.push(error);
                    if bound && !interpolation {
                        let error =
                            format!("Type mismatch: expected {expected}, got String (from r)");
                        cli.push(error.clone());
                        lsp.push(error);
                    }
                }
                assert_findings(&source, None, cli, lsp);
            }
        }
    }
}

#[test]
fn issue_4282_round1_module_arguments_keep_main_messages_and_counts() {
    for (expression, builtin, actual) in [
        ("split(',', 'a,b')", "split", "List<String>"),
        ("join('', [src.id])", "join", "String"),
        (r#""${''}${src.id}""#, "", "String"),
    ] {
        for (argument_type, expected) in [
            ("String", "String"),
            ("Int", "Int"),
            ("Bool", "Bool"),
            ("list(Int)", "List<Int>"),
        ] {
            let source = format!(
                "let src = mock.test.Resource {{}}\nlet r = {expression}\nlet template = use {{ source = './child' }}\nlet child = template {{ value = r }}\n"
            );
            let mut cli = vec![];
            let mut lsp = vec![];
            if !builtin.is_empty() {
                lsp.push(pipe_hint(builtin));
            }
            if builtin == "split" {
                lsp.push(
                    "Unused let binding 'src'. Consider using an anonymous resource instead."
                        .into(),
                );
            }
            if expected != "String" || builtin == "split" {
                if builtin.is_empty() {
                    let error = format!(
                        "module call 'child': argument 'value': expected {expected}, got String"
                    );
                    cli.push(error.clone());
                    lsp.push(error);
                } else {
                    let detail = if builtin == "split" && argument_type == "list(Int)" {
                        "missing expression evidence: list element 0".to_string()
                    } else {
                        format!(
                            "expected {expected}, got {}",
                            if builtin == "split" { "list" } else { "String" }
                        )
                    };
                    let error =
                        format!("Invalid argument type for 'value' in module 'template': {detail}");
                    cli.push(format!("Module resolution error: {error}"));
                    lsp.push(error);
                    let error = format!(
                        "module call 'child': argument 'value': cannot assign {actual} to 'value': expected {expected}, got {actual} (from r)"
                    );
                    cli.push(error.clone());
                    lsp.push(error);
                }
            }
            assert_findings(&source, Some(argument_type), cli, lsp);
        }
    }
}

#[test]
fn issue_4282_round1_integer_interpolation_checks_refinement_once() {
    for number in [5, 55] {
        let source = format!("let n = {number}\nmock.test.Resource {{ digit = \"${{n}}\" }}\n");
        let (cli, lsp) = if number == 5 {
            (vec![], vec![])
        } else {
            let error =
                "Invalid value '55' for 'digit': does not match required pattern /^[0-9]$/ (Full)";
            (
                vec![format!("mock.test.Resource.<pending>: {error}")],
                vec![error.into()],
            )
        };
        assert_findings(&source, None, cli, lsp);
    }
}

#[test]
fn issue_4282_round1_mixed_list_keeps_main_messages_and_counts() {
    for (expression, invalid) in [(r#"["${''}a", 'b']"#, false), (r#"["${''}A", 'b']"#, true)] {
        let source = format!("let r = {expression}\nmock.test.Resource {{ names = r }}\n");
        let messages = if invalid {
            vec!["List item at index 0: Invalid Name value 'A': does not match required pattern /^[a-z]+$/ (Full)".into()]
        } else {
            vec![]
        };
        assert_findings(&source, None, messages.clone(), messages);
    }
}

#[test]
fn issue_4282_round1_mixed_builtin_list_uses_main_interpolation_messages_and_counts() {
    for (expression, invalid) in [
        ("[join('', ['a']), 'b']", false),
        ("[join('', ['A']), 'b']", true),
        ("[join('', ['a']), plain]", false),
        ("[join('', ['A']), plain]", true),
    ] {
        for bound in [false, true] {
            let (declaration, assigned) = if bound {
                (format!("let r = {expression}\n"), "r")
            } else {
                (String::new(), expression)
            };
            let source = format!(
                "let plain = 'b'\n{declaration}mock.test.Resource {{ names = {assigned} }}\n"
            );
            let mut cli = vec![];
            let mut lsp = vec![pipe_hint("join")];
            if bound && invalid {
                let error = "List item at index 0: Invalid Name value 'A': does not match required pattern /^[a-z]+$/ (Full)";
                cli.push(error.into());
                lsp.push(error.into());
            } else if invalid {
                cli.push("mock.test.Resource.<pending>: List item at index 0: Invalid value 'A' for 'names' (Name): does not match required pattern /^[a-z]+$/ (Full)".into());
                lsp.push("Invalid value 'A' for 'names[0]' (Name): does not match required pattern /^[a-z]+$/ (Full)".into());
            }
            assert_findings(&source, None, cli, lsp);
        }
    }
}
