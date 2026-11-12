mod support;

use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
};
use support::fixture::{analyze, engine_with_schemas, write_fixture};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

fn schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    for (name, ty) in [
        ("Source", AttributeType::string()),
        (
            "Other",
            AttributeType::refined_string(
                Some(TypeIdentity::from_dotted("mock.Other")),
                None,
                None,
                None,
            ),
        ),
        (
            "Reference",
            AttributeType::reference(TypeIdentity::from_dotted("mock.OtherReference"), None, None),
        ),
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
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("mock.RecoveryProbe")),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas
}

#[test]
fn substituted_reference_diagnostics_match_unknown_source_wording_severity_and_range() {
    let engine = engine_with_schemas(schemas());
    for sibling in [false, true] {
        for source_type in ["Source", "Other", "Reference"] {
            let fixture = write_fixture(&[
                ("providers.crn", "provider mock {}\n"),
                ("backend.crn", "backend local { path = 'state.json' }\n"),
                ("sources.crn", ""),
                ("main.crn", ""),
            ]);
            let write = |value| {
                let source = format!("let a = mock.{source_type} {{ {value} }}\n");
                let consumer = "let sink = mock.Sink {\n  value = a.name\n}\n";
                std::fs::write(
                    fixture.path().join("sources.crn"),
                    if sibling { source.as_str() } else { "" },
                )
                .unwrap();
                std::fs::write(
                    fixture.path().join("main.crn"),
                    if sibling {
                        consumer.to_owned()
                    } else {
                        format!("{source}{consumer}")
                    },
                )
                .unwrap();
            };
            let findings = || {
                let mut diagnostics = analyze(&engine, &fixture, "main.crn");
                diagnostics
                    .retain(|diagnostic| !diagnostic.message.starts_with("Unused let binding"));
                diagnostics
            };
            write("");
            let unknown = findings();
            assert_eq!(
                unknown.len(),
                usize::from(source_type != "Reference"),
                "{source_type}: {unknown:#?}"
            );
            if let Some(diagnostic) = unknown.first() {
                assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::WARNING));
                assert_eq!(diagnostic.range.start.line, if sibling { 1 } else { 2 });
                assert_eq!(diagnostic.range.start.character, 2);
                assert!(!diagnostic.message.contains(" as "));
            }
            write("name = 'abc'");
            assert_eq!(findings(), unknown, "{source_type}, sibling={sibling}");
            for name in ["sources.crn", "providers.crn", "backend.crn"] {
                assert!(analyze(&engine, &fixture, name).is_empty(), "{name}");
            }
        }
    }
}

fn assert_reference_diagnostic_list(
    schemas: SchemaRegistry,
    value: &str,
    expected: impl Fn(u32, bool, bool) -> Vec<Diagnostic>,
) {
    let engine = engine_with_schemas(schemas);
    for sibling in [false, true] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider mock {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
            ("sources.crn", ""),
            ("main.crn", ""),
        ]);
        for known in [false, true] {
            let source = if known {
                "let a = mock.Source { name = 'abc' }\n"
            } else {
                "let a = mock.Source {}\n"
            };
            let consumer = format!("mock.Sink {{\n  value = {value}\n}}\n");
            std::fs::write(
                fixture.path().join("sources.crn"),
                if sibling { source } else { "" },
            )
            .unwrap();
            std::fs::write(
                fixture.path().join("main.crn"),
                if sibling {
                    consumer
                } else {
                    format!("{source}{consumer}")
                },
            )
            .unwrap();
            let line = if sibling { 1 } else { 2 };
            assert_eq!(
                analyze(&engine, &fixture, "main.crn"),
                expected(line, known, sibling),
                "value={value}, known={known}, sibling={sibling}",
            );
            for file in ["sources.crn", "providers.crn", "backend.crn"] {
                assert!(analyze(&engine, &fixture, file).is_empty(), "{file}");
            }
        }
    }
}

fn warning(line: u32, message: &str) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(line, 2), Position::new(line, 7)),
        severity: Some(DiagnosticSeverity::WARNING),
        source: Some("carina".into()),
        message: message.into(),
        ..Default::default()
    }
}

#[test]
fn secret_reference_keeps_exact_unknown_source_diagnostic_list() {
    assert_reference_diagnostic_list(schemas(), "secret(a.name)", |line, _, _| {
        vec![warning(
            line,
            "Type mismatch: expected mock.RecoveryProbe, got String",
        )]
    });
}

#[test]
fn resource_handle_reference_keeps_exact_diagnostic_list() {
    assert_reference_diagnostic_list(schemas(), "a", |line, _, _| {
        // A bare resource has both main's type error and its value-use guidance.
        [
            "expected mock.RecoveryProbe, got mock.Source",
            "'a' is a resource, not a value; reference one of its attributes, e.g. 'a.<attribute>'",
        ]
        .into_iter()
        .map(|message| Diagnostic {
            range: Range::new(Position::new(line, 10), Position::new(line, 11)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("carina".into()),
            message: message.into(),
            ..Default::default()
        })
        .collect()
    });
}

#[test]
fn reference_in_nested_map_list_keeps_exact_diagnostic_lists() {
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "value",
                    AttributeType::map(AttributeType::list(AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("mock.RecoveryProbe")),
                        None,
                        None,
                        None,
                    ))),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    assert_reference_diagnostic_list(schemas, "{ items = [a.name] }", |line, known, sibling| {
        // Preserve the current renderers' distinct prefixes: expression findings
        // own unknown/sibling values; recovery supplies the same-file known use.
        let message = if known && !sibling {
            "Type mismatch: expected mock.RecoveryProbe, got String (from a.name)"
        } else {
            "cannot assign String to 'value': expected mock.RecoveryProbe, got String (from a.name)"
        };
        vec![warning(line, message)]
    });
}
