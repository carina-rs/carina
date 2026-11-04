use super::*;
use carina_core::schema::{AttributeSchema, AttributeType, ListElements, ResourceSchema};

#[test]
fn for_expansion_reports_one_list_count_error_per_authored_token() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Collection")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::refined_list(
                        AttributeType::string(),
                        true,
                        Some((Some(0), Some(1))),
                        ListElements::AllowDuplicates,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let source = r#"let collections = for name in ["first", "second"] {
  mock.test.Collection {
    name = name
    params = ["ok", "ok"]
  }
}
"#;
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let document = create_document_at(source, path.clone());
    let diagnostics = engine.analyze(&document, Some(&path));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0]
            .message
            .contains("length 2 is outside allowed range [0, 1]")
    );
    assert_eq!(diagnostics[0].range.start.line, 3);
    assert_eq!(diagnostics[0].range.start.character, 4);
    assert_eq!(diagnostics[0].range.end.line, 3);
    assert_eq!(diagnostics[0].range.end.character, 10);
}

fn collection_diagnostics(field_type: AttributeType, value: &str) -> Vec<Diagnostic> {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Collection")
            .attribute(AttributeSchema::optional("params", field_type).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let prefix = if value.contains("producer") {
        "let producer = mock.test.Collection {}\n"
    } else {
        ""
    };
    let source = format!("{prefix}mock.test.Collection {{\n  params = {value}\n}}\n");
    let path = directory.path().join("main.crn");
    std::fs::write(&path, &source).unwrap();
    let document = create_document_at(&source, path.clone());
    engine.analyze(&document, Some(&path))
}

#[test]
fn list_count_has_one_core_error_in_directory() {
    for count in [10, 11] {
        let values = vec!["\"ok\""; count].join(", ");
        let diagnostics = collection_diagnostics(
            AttributeType::refined_list(
                AttributeType::string(),
                true,
                Some((Some(0), Some(10))),
                ListElements::AllowDuplicates,
            ),
            &format!("[{values}]"),
        );
        assert_eq!(
            diagnostics.len(),
            usize::from(count == 11),
            "{diagnostics:#?}"
        );
        if count == 11 {
            assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
            assert!(
                diagnostics[0]
                    .message
                    .contains("length 11 is outside allowed range [0, 10]")
            );
        }
    }
}

#[test]
fn refined_map_value_has_one_core_error_in_directory() {
    let diagnostics = collection_diagnostics(
        AttributeType::map(AttributeType::refined_string(
            None,
            None,
            Some((Some(0), Some(3))),
            None,
        )),
        "{ key = \"abcd\" }",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0]
            .message
            .contains("length 4 is outside allowed range [0, 3]")
    );
}

#[test]
fn value_report_keeps_equal_errors_at_distinct_computed_paths() {
    let diagnostics = collection_diagnostics(
        AttributeType::list(AttributeType::refined_string(
            None,
            None,
            Some((Some(0), Some(1))),
            None,
        )),
        "[\"bad\", \"bad\"] |> concat([])",
    );
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].range, diagnostics[1].range);
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostic.message.contains(&format!("params[{index}]")));
        assert!(
            diagnostic
                .message
                .contains("length 3 is outside allowed range [0, 1]")
        );
    }
}

#[test]
fn value_report_keeps_equal_coalesce_operand_findings() {
    let diagnostics =
        collection_diagnostics(AttributeType::string(), "(producer.params ?? 42) ?? 43");
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].range, diagnostics[1].range);
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostic.message.contains("expected String, got Int"));
    }
}

#[test]
fn value_report_keeps_multiple_leaves_duplicate_range_and_utf16() {
    use tower_lsp::lsp_types::{Position, Range};
    let value = "[\"🙂\", \"ok\", \"ok\", 5, 6]";
    let diagnostics = collection_diagnostics(
        AttributeType::refined_list(AttributeType::string(), true, None, ListElements::Unique),
        value,
    );
    assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
    let line = format!("  params = {value}");
    for token in ["5", "6", "\"ok\""] {
        let start = line[..line.rfind(token).unwrap()].encode_utf16().count() as u32;
        let expected = Range::new(
            Position::new(1, start),
            Position::new(1, start + token.encode_utf16().count() as u32),
        );
        let matching: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.range == expected)
            .collect();
        assert_eq!(matching.len(), 1, "{diagnostics:#?}");
        assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    }
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("expected String"))
            .count(),
        2
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("Duplicate list element"))
            .count(),
        1
    );
}

#[test]
fn value_report_keeps_map_key_findings() {
    let diagnostics = collection_diagnostics(
        AttributeType::map_with_key(
            AttributeType::refined_string(None, None, Some((None, Some(3))), None),
            AttributeType::string(),
        ),
        "{ first = \"ok\", second = \"ok\" }",
    );
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostic.message.contains("outside allowed range"));
    }
}

#[test]
fn value_report_keeps_nested_groups_read_only_and_syntax_hint() {
    use carina_core::schema::{FieldGroup, StructField};
    let diagnostics = collection_diagnostics(
        AttributeType::list(AttributeType::struct_with_field_groups(
            "Entry",
            vec![
                StructField::optional("allow", AttributeType::string()).build(),
                StructField::optional("deny", AttributeType::string()).build(),
                StructField::provider_populated("id", AttributeType::string()).build(),
            ],
            vec![FieldGroup::exactly_one(["allow", "deny"]).unwrap()],
        )),
        "[{ id = \"authored\" }]",
    );
    assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
    for message in ["exactly one of", "populated by the provider"] {
        let matching: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains(message))
            .collect();
        assert_eq!(matching.len(), 1, "{diagnostics:#?}");
        assert_eq!(matching[0].severity, Some(DiagnosticSeverity::ERROR));
    }
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::HINT))
            .count(),
        1
    );
}

#[test]
fn value_report_keeps_union_enum_payload_only_on_replaceable_token() {
    use carina_core::schema::TypeIdentity;
    let enum_type = AttributeType::enum_(
        TypeIdentity::bare("Mode"),
        Some(vec!["ok".into()]),
        vec![],
        None,
        None,
    );
    let diagnostics = collection_diagnostics(
        AttributeType::union(vec![
            AttributeType::refined_string(None, None, Some((None, Some(1))), None),
            enum_type.clone(),
        ]),
        "\"bad\"",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    let data = crate::code_action::EnumDiagnosticData::from_diagnostic(&diagnostics[0]).unwrap();
    assert!(data.has_non_enum_alternative);
    assert!(diagnostics[0].message.contains("outside allowed range"));
    assert!(
        diagnostics[0]
            .message
            .contains("expects an enum identifier")
    );
    for value in ["[bad]", "[bad] |> concat([])"] {
        let diagnostics = collection_diagnostics(AttributeType::list(enum_type.clone()), value);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            diagnostics[0].data.is_none(),
            "a collection cannot be replaced with one enum token"
        );
    }
}

#[test]
fn value_report_checks_authored_coalesce_defaults() {
    for value in ["producer.params ?? 42", "42 ?? producer.params"] {
        let diagnostics = collection_diagnostics(AttributeType::string(), value);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostics[0].message.contains("expected String, got Int"));
    }
}

#[test]
fn value_report_checks_static_functions_and_masks_secret_leaves() {
    let diagnostics = collection_diagnostics(
        AttributeType::list(AttributeType::refined_string(
            None,
            None,
            Some((None, Some(3))),
            None,
        )),
        "[[\"ab\", \"cd\"] |> join(\"\"), secret(\"private-value\")]",
    );
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    for diagnostic in &diagnostics {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostic.message.contains("outside allowed range"));
        assert!(!diagnostic.message.contains("private-value"));
    }
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("(secret)"))
            .count(),
        1
    );
}

#[test]
fn value_report_uses_unsaved_buffer_with_sibling_binding_authority() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Collection")
            .attribute(AttributeSchema::optional("params", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(directory.path().join("values.crn"), "let fallback = 42\n").unwrap();
    let path = directory.path().join("main.crn");
    std::fs::write(&path, "mock.test.Collection { params = \"ok\" }\n").unwrap();
    let buffer = "mock.test.Collection { params = fallback }\n";
    let document = create_document_at(buffer, path.clone());
    let diagnostics = engine.analyze(&document, Some(&path));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0].message.contains("String") && diagnostics[0].message.contains("Int"),
        "{diagnostics:#?}"
    );
    assert_eq!(
        diagnostics[0].range.start.character,
        buffer.find("fallback").unwrap() as u32
    );
}

#[test]
fn review_secret_bound_callback_runs_once() {
    use carina_core::resource::{ConcreteValue, Value};
    use carina_core::schema::{TypeIdentity, validator};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let leaf = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Counted"),
        None,
        None,
        validator(move |value| {
            assert!(
                matches!(value, Value::Concrete(ConcreteValue::String(text)) if text.expose_plaintext() == "ok")
            );
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        None,
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .as_data_source()
            .attribute(AttributeSchema::optional("values", AttributeType::list(leaf)).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    // Data-source inputs use the production value collector without the
    // separate managed-resource identity canonicalization pass.
    let source =
        "let input = \"ok\"\nlet sink = read mock.test.Sink { values = [secret(input)] }\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let findings = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    assert!(findings.is_empty(), "{findings:#?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn review_concrete_arithmetic_sink_has_one_owner() {
    let diagnostics = collection_diagnostics(AttributeType::string(), "length([\"a\"]) + 1");
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(diagnostics[0].message.contains("expected String, got Int"));
}

fn review_coalesce_diagnostics(sink: AttributeType, expression: &str) -> Vec<Diagnostic> {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Producer")
            .attribute(AttributeSchema::provider_populated("text", AttributeType::string()).build())
            .attribute(
                AttributeSchema::provider_populated(
                    "items",
                    AttributeType::list(AttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(AttributeSchema::optional("params", sink).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let source = format!(
        "let producer = mock.test.Producer {{}}\nmock.test.Sink {{\n  params = {expression}\n}}\n"
    );
    let path = directory.path().join("main.crn");
    std::fs::write(&path, &source).unwrap();
    engine.analyze(&create_document_at(&source, path.clone()), Some(&path))
}

#[test]
fn review_deferred_function_coalesce_checks_both_operands() {
    for expression in [
        "producer.text ?? length(producer.items)",
        "length(producer.items) ?? producer.text",
    ] {
        let diagnostics = review_coalesce_diagnostics(AttributeType::string(), expression);
        assert_eq!(diagnostics.len(), 1, "{expression}: {diagnostics:#?}");
        assert!(
            diagnostics[0].message.contains("String") && diagnostics[0].message.contains("Int"),
            "{diagnostics:#?}"
        );
    }
}

#[test]
fn review_coalesce_checks_compatibility_even_when_union_accepts_operands() {
    let diagnostics = review_coalesce_diagnostics(
        AttributeType::union(vec![AttributeType::string(), AttributeType::int()]),
        "producer.text ?? length(producer.items)",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(diagnostics[0].message.contains("String") && diagnostics[0].message.contains("Int"));
}

#[test]
fn review_secret_indexed_map_path_masks_key_and_retains_anchor() {
    let source = "[secret({ private_key = \"ok\" })]";
    let diagnostics = collection_diagnostics(
        AttributeType::list(AttributeType::map_with_key(
            AttributeType::refined_string(None, None, Some((None, Some(1))), None),
            AttributeType::string(),
        )),
        source,
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        !diagnostics[0].message.contains("private_key"),
        "{diagnostics:#?}"
    );
    assert!(diagnostics[0].message.contains("(secret)"));
}
