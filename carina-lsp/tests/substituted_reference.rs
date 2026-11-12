mod support;

use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
};
use support::fixture::{analyze, engine_with_schemas, write_fixture};
use tower_lsp::lsp_types::DiagnosticSeverity;

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
