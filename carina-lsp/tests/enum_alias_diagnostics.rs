mod support;

use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
};
use support::fixture::{analyze, engine_with_schemas, write_fixture};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};

fn diagnostic(
    start: (u32, u32),
    end: (u32, u32),
    severity: DiagnosticSeverity,
    message: String,
) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
        severity: Some(severity),
        source: Some("carina".into()),
        message,
        ..Default::default()
    }
}

fn enum_diagnostic(line: u32, attribute: &str) -> Diagnostic {
    let start = 5 + attribute.len() as u32;
    let mut diagnostic = diagnostic(
        (line, start),
        (line, start + 9),
        DiagnosticSeverity::WARNING,
        enum_message(attribute),
    );
    diagnostic.data = Some(serde_json::json!({
        "expected": [
            {"is_alias": false, "provider": "mock", "segments": [], "type_name": "Mode", "value": "active"},
            {"is_alias": false, "provider": "mock", "segments": [], "type_name": "Mode", "value": "passive"}
        ],
        "has_non_enum_alternative": false,
        "kind": "string_literal",
        "tag": "carina_enum_mismatch"
    }));
    diagnostic
}

fn enum_message(attribute: &str) -> String {
    format!(
        "'{attribute}' (Mode) expects an enum identifier, got a string literal 'invalid'. Use one of: mock.Mode.active, mock.Mode.passive"
    )
}

fn alias_report(line: u32) -> Diagnostic {
    diagnostic(
        (line, 2),
        (line, 6),
        DiagnosticSeverity::ERROR,
        enum_message("mode"),
    )
}

fn mixed_syntax(line: u32) -> Diagnostic {
    diagnostic(
        (line, 2),
        (line, 6),
        DiagnosticSeverity::ERROR,
        "Cannot use both 'mode' and 'modes' (they refer to the same attribute)".into(),
    )
}

fn mixed_syntax_report() -> Diagnostic {
    diagnostic(
        (0, 0),
        (3, 1),
        DiagnosticSeverity::ERROR,
        "mock.EnumAlias.<pending>: cannot use both 'mode' and 'modes' (they refer to the same attribute)".into(),
    )
}

#[test]
fn enum_block_alias_and_mixed_syntax_keep_main_diagnostics() {
    for union in [false, true] {
        let mut schemas = SchemaRegistry::new();
        let ty = AttributeType::enum_(
            TypeIdentity::from_dotted("mock.Mode"),
            Some(vec!["active".into(), "passive".into()]),
            Vec::new(),
            None,
            None,
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("EnumAlias")
                .attribute(
                    AttributeSchema::optional(
                        "modes",
                        if union {
                            AttributeType::union(vec![ty, AttributeType::int()])
                        } else {
                            ty
                        },
                    )
                    .with_block_name("mode")
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let engine = engine_with_schemas(schemas);
        // Captured with origin/main b8340bdf0's diagnostics module before the
        // R3 renderer changes. These literal-only fixtures cannot reach R3.
        // Keep main's authored-alias report even when it repeats the enum error.
        for (body, expected) in [
            (
                "  mode = 'invalid'\n",
                vec![enum_diagnostic(1, "mode"), alias_report(1)],
            ),
            ("  modes = 'invalid'\n", vec![enum_diagnostic(1, "modes")]),
            (
                "  mode = 'invalid'\n  modes = 'invalid'\n",
                vec![
                    mixed_syntax_report(),
                    mixed_syntax(1),
                    enum_diagnostic(2, "modes"),
                    alias_report(1),
                ],
            ),
            (
                "  modes = 'invalid'\n  mode = 'invalid'\n",
                vec![
                    mixed_syntax_report(),
                    enum_diagnostic(1, "modes"),
                    mixed_syntax(2),
                    alias_report(2),
                ],
            ),
        ] {
            let source = format!("mock.EnumAlias {{\n{body}}}\n");
            let fixture = write_fixture(&[
                ("providers.crn", "provider mock {}\n"),
                ("backend.crn", "backend local { path = 'state.json' }\n"),
                ("main.crn", &source),
            ]);
            let diagnostics = analyze(&engine, &fixture, "main.crn");
            assert_eq!(diagnostics, expected, "union={union}, body={body:?}");
            for file in ["providers.crn", "backend.crn"] {
                assert!(analyze(&engine, &fixture, file).is_empty(), "{file}");
            }
        }
    }
}

#[test]
fn substituted_enum_keeps_only_main_rich_diagnostic_and_reference_mismatch() {
    let mut source_message = None;
    for list in [false, true] {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Source")
                .attribute(AttributeSchema::optional("attr", AttributeType::string()).build())
                .attribute(
                    AttributeSchema::provider_populated("computed", AttributeType::string())
                        .build(),
                )
                .validate()
                .unwrap(),
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("StringSink")
                .attribute(AttributeSchema::optional("first", AttributeType::string()).build())
                .attribute(AttributeSchema::optional("second", AttributeType::string()).build())
                .validate()
                .unwrap(),
        );
        let mode = AttributeType::enum_(
            TypeIdentity::from_dotted("mock.Mode"),
            Some(vec!["active".into(), "passive".into()]),
            Vec::new(),
            None,
            None,
        );
        schemas.insert(
            "mock",
            ResourceSchema::new("EnumSink")
                .attribute(
                    AttributeSchema::optional(
                        "status",
                        if list {
                            AttributeType::list(mode)
                        } else {
                            mode
                        },
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let engine = engine_with_schemas(schemas);
        let fixture = write_fixture(&[
            ("providers.crn", "provider mock {}\n"),
            ("backend.crn", "backend local { path = 'state.json' }\n"),
            ("main.crn", ""),
        ]);
        let run = |value: &str| {
            let value = if list {
                format!("[{value}]")
            } else {
                value.to_owned()
            };
            std::fs::write(fixture.path().join("main.crn"), format!(
                "let other = mock.Source {{ attr = 'invalid' }}\nlet alias = 'invalid'\nmock.StringSink {{\n  first = alias\n  second = other.attr\n}}\nmock.EnumSink {{ status = {value} }}\n"
            )).unwrap();
            analyze(&engine, &fixture, "main.crn")
        };
        // A literal alias never enters R3, so this keeps main's rich enum
        // rendering and early continuation as the independent control.
        let ordinary = run("alias");
        assert_eq!(ordinary.len(), 1, "{ordinary:#?}");
        let unknown = run("other.computed");
        assert_eq!(unknown.len(), 1, "{unknown:#?}");
        if !list {
            source_message = Some(unknown[0].message.replace("other.computed", "other.attr"));
        }
        let known = run("other.attr");
        assert_eq!(known.len(), 2, "list={list}: {known:#?}");
        let enum_message = ordinary[0]
            .message
            .split_once("expects an enum identifier")
            .unwrap()
            .1;
        assert_eq!(
            known
                .iter()
                .filter(|finding| finding.message.ends_with(enum_message))
                .count(),
            1,
            "{known:#?}"
        );
        assert_eq!(
            known
                .iter()
                .filter(|finding| Some(&finding.message) == source_message.as_ref())
                .count(),
            1,
            "{known:#?}"
        );
    }
}
