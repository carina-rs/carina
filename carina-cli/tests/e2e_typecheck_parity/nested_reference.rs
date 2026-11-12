use super::*;
use carina_core::schema::TypeIdentity;

fn schemas(shape: &str) -> SchemaRegistry {
    let target = AttributeType::refined_string(
        Some(TypeIdentity::from_dotted("mock.RecoveryProbe")),
        None,
        None,
        None,
    );
    let value = match shape {
        "list" => AttributeType::list(target.clone()),
        "map" => AttributeType::map(target.clone()),
        "struct" => AttributeType::struct_(
            "Record",
            vec![StructField::required("field", target.clone()).build()],
        ),
        "ref" => AttributeType::ref_("Target"),
        "recursive" => AttributeType::ref_("Node"),
        _ => target.clone(),
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("value", value).build())
            .with_def("Target", target.clone())
            .with_def(
                "Node",
                AttributeType::struct_(
                    "Node",
                    vec![
                        StructField::required("field", target).build(),
                        StructField::optional(
                            "children",
                            AttributeType::list(AttributeType::ref_("Node")),
                        )
                        .build(),
                    ],
                ),
            )
            .validate()
            .unwrap(),
    );
    schemas
}

fn assert_shape(shape: &str, value: &str) {
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("sources.crn", "let a = mock.Source {}\n"),
        ("main.crn", ""),
    ]);
    let consumer = if shape == "module" {
        std::fs::create_dir(fixture.path().join("child")).unwrap();
        std::fs::write(
            fixture.path().join("child/arguments.crn"),
            "arguments { input: String }\n",
        )
        .unwrap();
        std::fs::write(
            fixture.path().join("child/main.crn"),
            "let sink = mock.Sink { value = input }\n",
        )
        .unwrap();
        "let child = use { source = './child' }\nlet instance = child { input = a.name }\n".into()
    } else {
        format!("let sink = mock.Sink {{ value = {value} }}\n")
    };
    std::fs::write(fixture.path().join("main.crn"), &consumer).unwrap();
    let schemas = schemas(shape);
    let engine = engine_with_schemas(schemas.clone());
    if shape != "module" {
        std::fs::write(
            fixture.path().join("main.crn"),
            consumer.replace("a.name", "'abc'"),
        )
        .unwrap();
        assert!(
            cli_diagnostics(factories_for(&schemas), &fixture).is_empty(),
            "literal {shape}"
        );
        assert!(
            lsp_diagnostics(&engine, &fixture, "main.crn")
                .iter()
                .all(|diagnostic| diagnostic.message.starts_with("Unused let binding")),
            "literal {shape}"
        );
        std::fs::write(fixture.path().join("main.crn"), &consumer).unwrap();
    }
    let run = |known| {
        std::fs::write(
            fixture.path().join("sources.crn"),
            if known {
                "let a = mock.Source { name = 'abc' }\n"
            } else {
                "let a = mock.Source {}\n"
            },
        )
        .unwrap();
        let cli = cli_diagnostics(factories_for(&schemas), &fixture);
        let mut lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
        if shape == "module" {
            lsp.extend(lsp_diagnostics(&engine, &fixture, "child/main.crn"));
        }
        lsp.retain(|diagnostic| !diagnostic.message.starts_with("Unused let binding"));
        (cli, lsp)
    };
    let unknown = run(false);
    let count = if shape == "module" { 3 } else { 1 };
    assert_eq!(unknown.0.len(), count, "unknown {shape}: {unknown:#?}");
    assert_eq!(unknown.1.len(), count, "unknown {shape}: {unknown:#?}");
    assert_eq!(run(true), unknown, "substituted {shape}");
}

#[test]
fn substituted_list_element_matches_unknown_cli_and_lsp() {
    assert_shape("list", "[a.name]");
}
#[test]
fn substituted_map_value_matches_unknown_cli_and_lsp() {
    assert_shape("map", "{ field = a.name }");
}
#[test]
fn substituted_struct_field_matches_unknown_cli_and_lsp() {
    assert_shape("struct", "{ field = a.name }");
}
#[test]
fn substituted_secret_matches_unknown_cli_and_lsp() {
    assert_shape("secret", "secret(a.name)");
}
#[test]
fn substituted_module_argument_matches_unknown_cli_and_lsp() {
    assert_shape("module", "a.name");
}
#[test]
fn substituted_ref_definition_matches_unknown_cli_and_lsp() {
    assert_shape("ref", "a.name");
}
#[test]
fn substituted_recursive_definition_matches_unknown_cli_and_lsp() {
    assert_shape("recursive", "{ field = a.name children = [] }");
}

#[test]
fn substituted_recursive_child_keeps_existing_cli_and_lsp_findings() {
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("sources.crn", "let a = mock.Source {}\n"),
        (
            "main.crn",
            "let sink = mock.Sink {\n  value = {\n    field = 'abc'\n    children = [{ field = a.name\n                  children = [] }]\n  }\n}\n",
        ),
    ]);
    let schemas = schemas("recursive");
    let engine = engine_with_schemas(schemas.clone());
    let analyze = || {
        let report = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories_for(&schemas)),
            Default::default(),
        );
        assert!(
            report
                .errors
                .iter()
                .all(|error| matches!(error, carina_cli::error::AppError::ExpressionType(_))),
            "{:#?}",
            report.errors
        );
        let cli = report
            .errors
            .iter()
            .map(|error| carina_cli::error::format_error_with_source_chain(error))
            .collect::<Vec<_>>();
        let mut lsp = lsp_diagnostics(&engine, &fixture, "main.crn");
        lsp.retain(|diagnostic| !diagnostic.message.starts_with("Unused let binding"));
        (cli, lsp)
    };
    let unknown = analyze();
    // Retained expression checking fails first; this CLI/LSP difference predates R3.
    let source_file = fixture.path().join("main.crn");
    assert_eq!(
        unknown.0,
        [
            format!(
                "{} at line 2\nexpected String, got collection",
                source_file.display()
            ),
            format!(
                "{} at line 1\nexpected String, got collection",
                source_file.display()
            ),
        ],
    );
    assert_eq!(
        unknown
            .1
            .iter()
            .map(|diagnostic| diagnostic.message.as_str())
            .collect::<Vec<_>>(),
        [
            "expected String, got collection",
            "expected String, got collection",
            "cannot assign String to 'value': expected mock.RecoveryProbe, got String (from a.name)",
        ],
    );
    std::fs::write(
        fixture.path().join("sources.crn"),
        "let a = mock.Source { name = 'abc' }\n",
    )
    .unwrap();
    let known = analyze();
    assert_eq!(known, unknown);
}

#[test]
fn substituted_constrained_identityless_sink_keeps_validate_and_lsp_acceptance() {
    use carina_core::schema::{StringIdentityLayer, StringMemberConstraints};
    let mut schemas = schemas("ref");
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::string_member(
                        StringIdentityLayer::new(None, None, None, None),
                        StringMemberConstraints::new(None, Some((None, Some(8)))),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let fixture = write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("sources.crn", ""),
        ("main.crn", "let sink = mock.Sink { value = a.name }\n"),
    ]);
    let engine = engine_with_schemas(schemas.clone());
    for source in [
        "let a = mock.Source {}\n",
        "let a = mock.Source { name = 'abc' }\n",
    ] {
        std::fs::write(fixture.path().join("sources.crn"), source).unwrap();
        assert!(cli_diagnostics(factories_for(&schemas), &fixture).is_empty());
        assert!(
            lsp_diagnostics(&engine, &fixture, "main.crn")
                .iter()
                .all(|diagnostic| diagnostic.message.starts_with("Unused let binding"))
        );
    }
}
