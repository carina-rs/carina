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
                        Some("^[a-z]+$".into()),
                        None,
                        None,
                    )),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "digit",
                    AttributeType::refined_string(None, Some("^[0-9]$".into()), None, None),
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

// Pinned on origin/main 980fff07f. These assertions also define the intended
// per-element path for builtin results; interpolation's messages stay fixed.
fn expected_findings(
    partial: bool,
    valid: bool,
    bound: bool,
    sink: &str,
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    if valid {
        return (BTreeMap::new(), BTreeMap::new());
    }
    let index = usize::from(partial);
    let value = if partial { "B" } else { "A" };
    let rule = "does not match required pattern /^[a-z]+$/";
    let (cli, lsp) = if sink == "resource" {
        if bound {
            let message =
                format!("List item at index {index}: Invalid Name value '{value}': {rule}");
            (vec![message.clone()], vec![message])
        } else {
            let cli = vec![format!(
                "mock.test.Resource.<pending>: List item at index {index}: Invalid value '{value}' for 'names' (Name): {rule}"
            )];
            let lsp = if partial {
                vec![format!("Invalid value 'B' for 'names[1]' (Name): {rule}")]
            } else {
                vec![
                    format!("Invalid value 'A' for 'names[0]' (Name): {rule}"),
                    format!("Invalid value 'B' for 'names[1]' (Name): {rule}"),
                ]
            };
            (cli, lsp)
        }
    } else {
        let detail = format!(
            "at list element {index}: expected mock.test.Resource.Name, got string '{value}': mock rejected `mock.test.Resource.Name`: Invalid Name value '{value}': {rule}"
        );
        if sink == "annotation" {
            (vec![format!("attribute 'checked': {detail}")], vec![detail])
        } else {
            let message = format!("module call 'child': argument 'value': {detail}");
            (vec![message.clone()], vec![message])
        }
    };
    (
        message_multiset(cli.iter().map(String::as_str)),
        message_multiset(lsp.iter().map(String::as_str)),
    )
}

fn assert_list_case(expression: &str, partial: bool, valid: bool, bound: bool, sink: &str) {
    let (declaration, rhs) = if bound {
        (format!("let r = {expression}\n"), "r")
    } else {
        (String::new(), expression)
    };
    let use_site = match sink {
        "resource" => format!("mock.test.Resource {{ names = {rhs} }}\n"),
        "annotation" => {
            format!("attributes {{ checked: list(mock.test.Resource.Name) = {rhs} }}\n")
        }
        _ => format!(
            "let template = use {{ source = './child' }}\nlet child = template {{ value = {rhs} }}\n"
        ),
    };
    let prefix = if partial {
        "let src = mock.test.Resource {}\n"
    } else {
        ""
    };
    let source = format!("{prefix}{declaration}{use_site}");
    let mut actual = findings(
        &source,
        (sink == "module").then_some("list(mock.test.Resource.Name)"),
    );
    actual
        .1
        .retain(|message, _| !message.starts_with("Consider using pipe form"));
    assert_eq!(
        actual,
        expected_findings(partial, valid, bound, sink),
        "{source}"
    );
}

fn assert_interpolation_baseline(sink: &str) {
    for (expression, partial, valid) in [
        (r#"["${''}A", "${''}B"]"#, false, false),
        (r#"["${src.id}-a", "${''}B"]"#, true, false),
        (r#"["${''}a", "${''}b"]"#, false, true),
    ] {
        for bound in [false, true] {
            assert_list_case(expression, partial, valid, bound, sink);
        }
    }
}

#[test]
fn issue_4282_round2_interpolation_lists_keep_main_resource_diagnostics() {
    assert_interpolation_baseline("resource");
}

#[test]
fn issue_4282_round2_interpolation_lists_keep_main_annotation_diagnostics() {
    assert_interpolation_baseline("annotation");
}

#[test]
fn issue_4282_round2_interpolation_lists_keep_main_module_diagnostics() {
    assert_interpolation_baseline("module");
}

#[test]
fn issue_4282_round2_builtin_lists_use_mains_interpolation_element_path() {
    for sink in ["resource", "annotation", "module"] {
        for bound in [false, true] {
            for (expression, partial, valid) in [
                ("split(',', 'A,B')", false, false),
                ("[join('', ['A']), upper('b')]", false, false),
                ("[join('', [src.id, '-a']), upper('b')]", true, false),
                ("split(',', 'a,b')", false, true),
                ("[join('', ['a']), lower('B')]", false, true),
            ] {
                assert_list_case(expression, partial, valid, bound, sink);
            }
        }
    }
}
