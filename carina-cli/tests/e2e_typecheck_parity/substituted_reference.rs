use super::*;
use carina_core::schema::TypeIdentity;

fn schemas() -> SchemaRegistry {
    let named = |name| {
        AttributeType::refined_string(Some(TypeIdentity::from_dotted(name)), None, None, None)
    };
    let mut schemas = SchemaRegistry::new();
    for (name, ty) in [
        ("Source", AttributeType::string()),
        ("Other", named("mock.Other")),
        (
            "Reference",
            AttributeType::reference(TypeIdentity::from_dotted("mock.OtherReference"), None, None),
        ),
        ("Identity", named("mock.RecoveryProbe")),
    ] {
        schemas.insert(
            "mock",
            ResourceSchema::new(name)
                .attribute(AttributeSchema::optional("name", ty.clone()).build())
                .attribute(AttributeSchema::provider_populated("computed", ty).build())
                .validate()
                .unwrap(),
        );
    }
    for (name, ty) in [
        ("Sink", named("mock.RecoveryProbe")),
        (
            "ReferenceSink",
            AttributeType::reference(
                TypeIdentity::from_dotted("mock.TargetReference"),
                None,
                None,
            ),
        ),
    ] {
        schemas.insert(
            "mock",
            ResourceSchema::new(name)
                .attribute(AttributeSchema::optional("value", ty).build())
                .validate()
                .unwrap(),
        );
    }
    schemas
}

fn fixture(source: &str, consumer: &str) -> TempDir {
    write_fixture(&[
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
        ("sources.crn", source),
        ("main.crn", consumer),
    ])
}

fn findings(
    fixture: &TempDir,
    schemas: &SchemaRegistry,
) -> (Vec<String>, Vec<tower_lsp::lsp_types::Diagnostic>) {
    let cli = cli_diagnostics(factories_for(schemas), fixture);
    let engine = engine_with_schemas(schemas.clone());
    let mut lsp = lsp_diagnostics(&engine, fixture, "main.crn");
    lsp.retain(|diagnostic| !diagnostic.message.starts_with("Unused let binding"));
    for sibling in ["providers.crn", "sources.crn", "backend.crn"] {
        let diagnostics = lsp_diagnostics(&engine, fixture, sibling);
        assert!(
            diagnostics.iter().all(|diagnostic| diagnostic
                .message
                .starts_with("Unused let binding")
                || diagnostic.severity == Some(DiagnosticSeverity::HINT)),
            "{sibling}: {diagnostics:#?}"
        );
    }
    (cli, lsp)
}

#[test]
fn substituted_reference_known_and_unknown_have_identical_cli_lsp_findings() {
    let schemas = schemas();
    for source in ["Source", "Other", "Reference", "Identity"] {
        for sink in ["Sink", "ReferenceSink"] {
            for sibling in [false, true] {
                let consumer = format!("let sink = mock.{sink} {{ value = a.name }}\n");
                let fixture = fixture("", "");
                let write = |value| {
                    let source = format!("let a = mock.{source} {{ {value} }}\n");
                    std::fs::write(
                        fixture.path().join("sources.crn"),
                        if sibling { source.as_str() } else { "" },
                    )
                    .unwrap();
                    std::fs::write(
                        fixture.path().join("main.crn"),
                        if sibling {
                            consumer.clone()
                        } else {
                            format!("{source}{consumer}")
                        },
                    )
                    .unwrap();
                };
                write("");
                let unknown = findings(&fixture, &schemas);
                let count = usize::from(sink == "Sink" && matches!(source, "Source" | "Other"));
                assert_eq!(unknown.0.len(), count, "{source} -> {sink}: {unknown:#?}");
                assert_eq!(unknown.1.len(), count, "{source} -> {sink}: {unknown:#?}");
                write("name = 'abc'");
                let known = findings(&fixture, &schemas);
                assert_eq!(known, unknown, "{source} -> {sink}, sibling={sibling}");
            }
        }
    }
}

#[test]
fn substituted_reference_same_file_and_alias_match_unknown_source_diagnostics() {
    let schemas = schemas();
    for value in ["a.name", "alias", "(a.name)"] {
        let alias = if value == "alias" {
            "let alias = a.name\n"
        } else {
            ""
        };
        let fixture = fixture("", "");
        let write = |source| {
            std::fs::write(fixture.path().join("main.crn"), format!(
            "let a = mock.Source {{ {source} }}\n{alias}let sink = mock.Sink {{ value = {value} }}\n"
        )).unwrap()
        };
        write("");
        let unknown = findings(&fixture, &schemas);
        assert_eq!(unknown.0.len(), 1, "{value}: {unknown:#?}");
        assert_eq!(unknown.1.len(), 1, "{value}: {unknown:#?}");
        write("name = 'abc'");
        assert_eq!(findings(&fixture, &schemas), unknown, "{value}");
    }
}

#[test]
fn substituted_reference_literals_and_discarded_branches_keep_main_acceptance() {
    let schemas = schemas();
    for sink in ["Sink", "ReferenceSink"] {
        for value in [
            "'abc'",
            "literal",
            "alias",
            "'abc' ?? a.name",
            "if true { 'abc' } else { a.name }",
            "('abc' ?? a.name) ?? a.computed",
        ] {
            let fixture = fixture(
                "let literal = 'abc'\nlet alias = literal\nlet a = mock.Source { name = 'abc' }\n",
                &format!("mock.{sink} {{ value = {value} }}\n"),
            );
            let actual = findings(&fixture, &schemas);
            assert!(
                actual.0.is_empty() && actual.1.is_empty(),
                "{sink}: {value}: {actual:#?}"
            );
        }
    }
}

#[test]
fn substituted_reference_preserves_literal_value_errors_alongside_reference_errors() {
    use carina_core::schema::{
        PatternMatch, PatternSpec, StringIdentityLayer, StringMemberConstraints,
    };
    let mut schemas = schemas();
    for (name, ty) in [
        (
            "PatternSink",
            AttributeType::string_member(
                StringIdentityLayer::new(
                    Some(TypeIdentity::from_dotted("mock.RecoveryProbe")),
                    None,
                    None,
                    None,
                ),
                StringMemberConstraints::new(
                    Some(PatternSpec::new("abc", PatternMatch::Full)),
                    None,
                ),
            ),
        ),
        (
            "LengthSink",
            AttributeType::string_member(
                StringIdentityLayer::new(
                    Some(TypeIdentity::from_dotted("mock.RecoveryProbe")),
                    None,
                    None,
                    None,
                ),
                StringMemberConstraints::new(None, Some((None, Some(2)))),
            ),
        ),
        (
            "CallbackSink",
            AttributeType::refined_string_with_validator(
                TypeIdentity::from_dotted("mock.CallbackProbe"),
                None,
                None,
                legacy_validator(|_| Err("slot callback rejected".into())),
                None,
            ),
        ),
    ] {
        schemas.insert(
            "mock",
            ResourceSchema::new(name)
                .attribute(AttributeSchema::optional("value", ty).build())
                .validate()
                .unwrap(),
        );
        for source in ["Identity", "Source", "Reference"] {
            let source_text = format!("let a = mock.{source} {{ name = 'bad' }}\n");
            let fixture = fixture(
                "",
                &format!("{source_text}let sink = mock.{name} {{ value = 'bad' }}\n"),
            );
            let literal = findings(&fixture, &schemas);
            assert_eq!(literal.0.len(), 1, "{name}: {literal:#?}");
            assert_eq!(literal.1.len(), 1, "{name}: {literal:#?}");
            std::fs::write(
                fixture.path().join("main.crn"),
                format!("{source_text}let sink = mock.{name} {{ value = a.computed }}\n"),
            )
            .unwrap();
            let unknown = findings(&fixture, &schemas);
            std::fs::write(
                fixture.path().join("main.crn"),
                format!("{source_text}let sink = mock.{name} {{ value = a.name }}\n"),
            )
            .unwrap();
            let substituted = findings(&fixture, &schemas);
            assert_eq!(
                substituted
                    .0
                    .iter()
                    .filter(|error| *error == &literal.0[0])
                    .count(),
                1,
                "{source} -> {name}: {substituted:#?}"
            );
            assert_eq!(
                substituted
                    .1
                    .iter()
                    .filter(|error| error.message == literal.1[0].message)
                    .count(),
                1,
                "{source} -> {name}: {substituted:#?}"
            );
            assert_eq!(
                substituted.0.len(),
                literal.0.len() + unknown.0.len(),
                "{source} -> {name}: {substituted:#?}"
            );
            assert_eq!(
                substituted.1.len(),
                literal.1.len() + unknown.1.len(),
                "{source} -> {name}: {substituted:#?}"
            );
            for reference in unknown.0 {
                assert!(
                    substituted
                        .0
                        .iter()
                        .any(|message| *message == reference.replace("a.computed", "a.name")),
                    "{substituted:#?}"
                );
            }
            for reference in unknown.1 {
                assert!(
                    substituted.1.iter().any(|diagnostic| diagnostic.message
                        == reference.message.replace("a.computed", "a.name")),
                    "{substituted:#?}"
                );
            }
        }
    }
}

#[test]
fn substituted_reference_preserves_enum_membership_diagnostics() {
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        ResourceSchema::new("EnumSink")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::enum_(
                        TypeIdentity::from_dotted("mock.Choice"),
                        Some(vec!["abc".into()]),
                        Vec::new(),
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let source = "let a = mock.Source { name = 'bad' }\n";
    let fixture = fixture(
        "",
        &format!("{source}let sink = mock.EnumSink {{ value = 'bad' }}\n"),
    );
    let literal = findings(&fixture, &schemas);
    assert_eq!(literal.0.len(), 1, "{literal:#?}");
    assert_eq!(literal.1.len(), 1, "{literal:#?}");
    std::fs::write(
        fixture.path().join("main.crn"),
        format!("{source}let sink = mock.EnumSink {{ value = a.computed }}\n"),
    )
    .unwrap();
    let unknown = findings(&fixture, &schemas);
    std::fs::write(
        fixture.path().join("main.crn"),
        format!("{source}let sink = mock.EnumSink {{ value = a.name }}\n"),
    )
    .unwrap();
    let actual = findings(&fixture, &schemas);
    assert_eq!(
        actual
            .0
            .iter()
            .filter(|message| **message == literal.0[0])
            .count(),
        1,
        "actual={actual:#?}; literal={literal:#?}; unknown={unknown:#?}"
    );
    assert_eq!(
        actual
            .1
            .iter()
            .filter(|diagnostic| literal.0[0].ends_with(&diagnostic.message))
            .count(),
        1,
        "actual={actual:#?}; literal={literal:#?}; unknown={unknown:#?}"
    );
    assert_eq!(
        actual.0.len(),
        literal.0.len() + unknown.0.len(),
        "actual={actual:#?}; literal={literal:#?}; unknown={unknown:#?}"
    );
    assert_eq!(
        actual.1.len(),
        literal.1.len() + unknown.1.len(),
        "actual={actual:#?}; literal={literal:#?}; unknown={unknown:#?}"
    );
}
