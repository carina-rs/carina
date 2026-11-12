use super::*;
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, StructField};
use tower_lsp::lsp_types::{DiagnosticSeverity, Position, Range};

fn ordinary_alias_diagnostics(mixed: bool) -> Vec<(Range, Option<DiagnosticSeverity>, String)> {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        directory.path().join("source.crn"),
        "let source = mock.Source {}\n",
    )
    .unwrap();
    let source = if mixed {
        "mock.Sink {\n    rule = source.output + 1\n    rules = []\n}\n"
    } else {
        "mock.Sink {\n    rule = source.output + 1\n}\n"
    };
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "rules",
                    AttributeType::list(AttributeType::struct_(
                        "Rule",
                        vec![StructField::optional("value", AttributeType::int()).build()],
                    )),
                )
                .with_block_name("rule")
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let document = create_document_at(source, path.clone());
    engine
        .analyze(&document, Some(&path))
        .into_iter()
        .map(|diagnostic| {
            (
                diagnostic.range,
                diagnostic.severity,
                diagnostic
                    .message
                    .replace(directory.path().to_str().unwrap(), "<directory>"),
            )
        })
        .collect()
}

fn diagnostic(
    start: (u32, u32),
    end: (u32, u32),
    severity: DiagnosticSeverity,
    message: &str,
) -> (Range, Option<DiagnosticSeverity>, String) {
    (
        Range::new(Position::new(start.0, start.1), Position::new(end.0, end.1)),
        Some(severity),
        message.into(),
    )
}

const MISMATCH: &str =
    "Type mismatch: expected List<Struct(Rule)>, got Int (from source.output + 1)";
const REPORT_MISMATCH: &str = "<directory>/main.crn:2:26: Type mismatch: expected List<Struct(Rule)>, got Int (from source.output + 1)";

// Expectations verified against origin/main af93e9ac8. Only the temporary
// directory prefix is normalized. Main's expression pass precedes the schema
// pass; its final report loop is deliberately preserved even when it repeats
// an ordinary finding. The symbolic loop records canonical `rules`, while
// the final error is labelled by the authored alias `rule`.
#[test]
fn round8_ordinary_block_alias_keeps_main_diagnostics() {
    assert_eq!(
        ordinary_alias_diagnostics(false),
        vec![
            diagnostic(
                (1, 11),
                (1, 28),
                DiagnosticSeverity::ERROR,
                "expected list(struct { value: Int }), got Int"
            ),
            diagnostic((1, 25), (1, 26), DiagnosticSeverity::ERROR, MISMATCH),
            diagnostic((1, 4), (1, 8), DiagnosticSeverity::ERROR, REPORT_MISMATCH),
        ]
    );
}

#[test]
fn round8_mixed_syntax_continue_keeps_main_report_error() {
    // The alias's mixed-syntax check continues before its symbolic loop.
    // Main still renders its report error after the two mixed-syntax findings.
    assert_eq!(
        ordinary_alias_diagnostics(true),
        vec![
            diagnostic(
                (1, 11),
                (1, 28),
                DiagnosticSeverity::ERROR,
                "expected list(struct { value: Int }), got Int"
            ),
            diagnostic(
                (0, 0),
                (3, 1),
                DiagnosticSeverity::ERROR,
                "mock.Sink.<pending>: cannot use both 'rule' and 'rules' (they refer to the same attribute)"
            ),
            diagnostic(
                (1, 4),
                (1, 8),
                DiagnosticSeverity::ERROR,
                "Cannot use both 'rule' and 'rules' (they refer to the same attribute)"
            ),
            diagnostic((1, 4), (1, 8), DiagnosticSeverity::ERROR, REPORT_MISMATCH),
            diagnostic(
                (2, 4),
                (2, 9),
                DiagnosticSeverity::HINT,
                "Prefer block syntax for 'rules'. Use `rules { ... }` instead of `rules = [{ ... }]`."
            ),
        ]
    );
}
