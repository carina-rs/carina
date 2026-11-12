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
