use super::*;
use carina_core::schema::{AttributeSchema, AttributeType, ListElements, ResourceSchema};

fn round7_collection_diagnostics(expression: &str) -> (String, Vec<Diagnostic>) {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Producer")
            .attribute(AttributeSchema::provider_populated("number", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Sink")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::refined_list(
                        AttributeType::string(),
                        true,
                        Some((None, Some(2))),
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
    let source = format!(
        "let producer = mock.test.Producer {{}}\nmock.test.Sink {{ params = {expression} }}\n"
    );
    let path = directory.path().join("main.crn");
    std::fs::write(&path, &source).unwrap();
    let diagnostics = engine.analyze(&create_document_at(&source, path.clone()), Some(&path));
    (source, diagnostics)
}

#[test]
fn round7_collection_child_failure_keeps_siblings_and_bounds() {
    let (source, diagnostics) = round7_collection_diagnostics("[producer.number, 1, 2]");
    for token in ["1", "2"] {
        let column = source.lines().nth(1).unwrap().find(token).unwrap() as u32;
        let matches: Vec<_> = diagnostics
            .iter()
            .filter(|diagnostic| {
                diagnostic.range
                    == Range::new(Position::new(1, column), Position::new(1, column + 1))
            })
            .collect();
        assert_eq!(matches.len(), 1, "{diagnostics:#?}");
        assert_eq!(matches[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(matches[0].message.contains("expected String, got Int"));
    }
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("length 3"))
            .count(),
        1,
        "{diagnostics:#?}"
    );
    let reference = source
        .lines()
        .nth(1)
        .unwrap()
        .find("producer.number")
        .unwrap() as u32;
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.range
                == Range::new(
                    Position::new(1, reference),
                    Position::new(1, reference + 15)
                ))
            .count(),
        1,
        "{diagnostics:#?}"
    );
    assert_eq!(diagnostics.len(), 4, "{diagnostics:#?}");
}

#[test]
fn round7_secret_collection_child_failure_keeps_siblings_and_bounds() {
    let (_, diagnostics) = round7_collection_diagnostics("secret([producer.number, 1, 2])");
    assert_eq!(diagnostics.len(), 4, "{diagnostics:#?}");
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("expected String, got Int"))
            .count(),
        3,
        "{diagnostics:#?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("length 3"))
            .count(),
        1,
        "{diagnostics:#?}"
    );
}

#[test]
fn round7_nested_folded_coalesce_keeps_rejection() {
    let diagnostics = collection_diagnostics(
        AttributeType::list(AttributeType::string()),
        r#"["ok" ?? 42]"#,
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0].message.contains("String") && diagnostics[0].message.contains("Int"),
        "{diagnostics:#?}"
    );
}

#[test]
fn round7_union_map_key_keeps_checks_independent_of_value_bindings() {
    let key = AttributeType::refined_string(None, None, Some((None, Some(3))), None);
    let diagnostics = round2_source_diagnostics(
        AttributeType::map_with_key(
            AttributeType::union(vec![key.clone(), key]),
            AttributeType::string(),
        ),
        "let bad = 42\nmock.test.Collection { params = { oversize = bad } }\n",
    );
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("length 8"))
            .count(),
        1,
        "{diagnostics:#?}"
    );
    assert_eq!(
        diagnostics
            .iter()
            .filter(|diagnostic| diagnostic.message.contains("expected String, got Int"))
            .count(),
        1,
        "{diagnostics:#?}"
    );
}

#[test]
fn round6_expanded_unique_lists_share_the_authored_duplicate_rule() {
    for (items, expected) in [
        (r#"[["a", "a"], ["b", "c", "b"]]"#, 1),
        (r#"[["a", "b", "a"], ["b", "c", "c"]]"#, 1),
        (r#"[["a", "a", "a"], ["b", "c", "b"]]"#, 2),
    ] {
        let source = format!(
            "let copies = for x in {items} {{\n  mock.test.Collection {{ params = x }}\n}}\n"
        );
        let diagnostics = round2_source_diagnostics(
            AttributeType::refined_list(AttributeType::string(), true, None, ListElements::Unique),
            &source,
        );
        assert_eq!(diagnostics.len(), expected, "{source}: {diagnostics:#?}");
        for diagnostic in diagnostics {
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            assert!(
                diagnostic.message.contains("Duplicate list element"),
                "{diagnostic:#?}"
            );
            let column = source.lines().nth(1).unwrap().rfind('x').unwrap() as u32;
            assert_eq!(
                diagnostic.range,
                Range::new(Position::new(1, column), Position::new(1, column + 1))
            );
        }
    }
}

#[test]
fn round6_evaluated_coalesce_preserves_operand_compatibility() {
    let diagnostics = collection_diagnostics(
        AttributeType::union(vec![AttributeType::int(), AttributeType::string()]),
        r#"length(["a"]) ?? "fallback""#,
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::WARNING));
    assert!(
        diagnostics[0].message.contains("expected Int, got String")
            && diagnostics[0].message.contains("coalesce default"),
        "{diagnostics:#?}"
    );
}

#[test]
fn round6_coalesce_concrete_item_failure_has_one_owner() {
    for expression in [
        r#"producer.items ?? [1, "ok"]"#,
        r#"producer.items ?? (["ok"] ?? [1, "ok"])"#,
    ] {
        let diagnostics =
            review_coalesce_diagnostics(AttributeType::list(AttributeType::string()), expression);
        assert_eq!(diagnostics.len(), 1, "{expression}: {diagnostics:#?}");
        assert_eq!(
            diagnostics[0].severity,
            Some(DiagnosticSeverity::ERROR),
            "{expression}: {diagnostics:#?}"
        );
        assert!(diagnostics[0].message.contains("expected String, got Int"));
    }
}

#[test]
fn round5_generated_list_indexes_share_the_authored_binding_path() {
    for (items, expected) in [
        ("[[1, \"ok\"], [\"ok\", 1]]", 1),
        ("[[1, 2], [3, \"ok\"]]", 2),
    ] {
        let source = format!(
            "let copies = for x in {items} {{\n  mock.test.Collection {{ params = x }}\n}}\n"
        );
        let diagnostics =
            round2_source_diagnostics(AttributeType::list(AttributeType::string()), &source);
        assert_eq!(diagnostics.len(), expected, "{source}: {diagnostics:#?}");
        for diagnostic in diagnostics {
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            assert!(
                diagnostic.message.contains("expected String"),
                "{diagnostic:#?}"
            );
        }
    }
}

#[test]
fn round5_binding_scalar_extent_does_not_change_authored_rule_identity() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::string(),
        "let copies = for x in [1, []] {\n  mock.test.Collection { params = x }\n}\n",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(diagnostics[0].data.is_none());
}

#[test]
fn round5_binding_token_messages_keep_each_list_leaf_path() {
    let source = "let xs = [5, 6]\nmock.test.Collection { params = xs }\n";
    let diagnostics =
        round2_source_diagnostics(AttributeType::list(AttributeType::string()), source);
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            diagnostic.message.contains(&format!("params[{index}]")),
            "{diagnostic:#?}"
        );
        let column = source.lines().nth(1).unwrap().find("xs").unwrap() as u32;
        assert_eq!(
            diagnostic.range,
            Range::new(Position::new(1, column), Position::new(1, column + 2))
        );
    }
    assert_ne!(diagnostics[0].message, diagnostics[1].message);
}

#[test]
fn round5_coalesce_keeps_empty_list_inference_failure() {
    let diagnostics = review_coalesce_diagnostics(
        AttributeType::union(vec![
            AttributeType::string(),
            AttributeType::list(AttributeType::string()),
        ]),
        "producer.text ?? []",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        diagnostics[0].message.contains("empty collection"),
        "{diagnostics:#?}"
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn round5_child_module_expansions_merge_authored_value_rules() {
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
    let child = directory.path().join("child");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(
        child.join("arguments.crn"),
        "arguments { label: String = \"ok\" }\n",
    )
    .unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    std::fs::write(
        child.join("main.crn"),
        "let copies = for x in [1, false] {\n  mock.test.Collection { params = x }\n}\n",
    )
    .unwrap();
    let source = "let child = use { source = \"./child\" }\nchild {}\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0].message.contains("expected String"),
        "{diagnostics:#?}"
    );
    assert_eq!(diagnostics[0].range.start.line, 1, "{diagnostics:#?}");
}

#[test]
fn round4_orphan_coalesce_enum_shadow_has_only_authority_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Collection")
            .attribute(
                AttributeSchema::optional(
                    "params",
                    AttributeType::enum_(
                        carina_core::schema::TypeIdentity::new(Some("mock"), ["test"], "Event"),
                        Some(vec!["viewer_request".into(), "ok".into()]),
                        vec![],
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let source = "provider mock {}\nlet viewer_request = 5\nmock.test.Collection { params = viewer_request ?? ok }\n";
    let diagnostics = engine.analyze(&create_document(source), None);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0]
            .message
            .contains("was interpreted as the `let` binding"),
        "{diagnostics:#?}"
    );
}

#[test]
fn round4_conditional_lists_evaluate_static_arithmetic_in_both_branches() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Collection")
            .attribute(
                AttributeSchema::optional("params", AttributeType::list(AttributeType::string()))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("arguments.crn"),
        "arguments { flag: Bool }\n",
    )
    .unwrap();
    let source = "mock.test.Collection { params = if flag { [length(\"a\") + 2] } else { [length(\"abc\") + 4] } }\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(
            diagnostic.message.contains("expected String, got Int"),
            "{diagnostic:#?}"
        );
    }
}

#[test]
fn round4_preserved_collections_resolve_nested_calls() {
    for (value, count) in [
        ("[length(producer.params)]", 1),
        ("[\"ok\" ?? length(producer.params)]", 1),
        ("[length(producer.params) ?? length(producer.params)]", 2),
    ] {
        let diagnostics = round2_source_diagnostics(
            AttributeType::list(AttributeType::string()),
            &format!(
                "let producer = mock.test.Collection {{ params = [\"known\"] }}\nmock.test.Collection {{ params = {value} }}\n"
            ),
        );
        assert_eq!(diagnostics.len(), count, "{value}: {diagnostics:#?}");
        for diagnostic in diagnostics {
            assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
            assert!(
                diagnostic.message.contains("expected String, got Int"),
                "{diagnostic:#?}"
            );
            assert!(diagnostic.message.contains("params[0]"), "{diagnostic:#?}");
        }
    }
}

#[test]
fn round4_expansion_keeps_complementary_coalesce_failures() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::refined_string(None, None, Some((None, Some(3))), None),
        "let copies = for x in [[\"abcd\", \"ok\"], [\"ok\", \"abcd\"]] {\n  mock.test.Collection { params = x[0] ?? x[1] }\n}\n",
    );
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    for diagnostic in diagnostics {
        assert_eq!(diagnostic.severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostic.message.contains("length 4"), "{diagnostic:#?}");
    }
}

#[test]
fn round4_expansion_merges_computed_map_key_failures() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::map_with_key(
            AttributeType::refined_string(None, None, Some((None, Some(3))), None),
            AttributeType::string(),
        ),
        "let copies = for x in [{ first = \"ok\" }, { second = \"ok\" }] {\n  mock.test.Collection { params = x }\n}\n",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0].message.contains("outside allowed range"),
        "{diagnostics:#?}"
    );
}

fn round2_source_diagnostics(field_type: AttributeType, source: &str) -> Vec<Diagnostic> {
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
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    engine.analyze(&create_document_at(source, path.clone()), Some(&path))
}

#[test]
fn round3_coalesce_binding_sources_keep_each_operand() {
    for value in ["good ?? bad", "bad ?? good", "[good ?? bad]"] {
        let leaf = AttributeType::refined_string(None, None, Some((None, Some(1))), None);
        let field = if value.starts_with('[') {
            AttributeType::list(leaf)
        } else {
            leaf
        };
        let source = format!(
            "let good = \"a\"\nlet bad = \"abcd\"\nmock.test.Collection {{ params = {value} }}\n"
        );
        let diagnostics = round2_source_diagnostics(field, &source);
        assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:#?}");
        assert!(
            diagnostics[0].message.contains("length 4"),
            "{diagnostics:#?}"
        );
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        let bad_column = source.lines().last().unwrap().find("bad").unwrap() as u32;
        assert_eq!(
            diagnostics[0].range.start.character, bad_column,
            "{diagnostics:#?}"
        );
    }
}

#[test]
fn round3_known_reference_keeps_invalid_coalesce_default() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::string(),
        "let producer = mock.test.Collection { params = \"known\" }\nmock.test.Collection { params = producer.params ?? 42 }\n",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0].message.contains("expected String, got Int"),
        "{diagnostics:#?}"
    );
}

#[test]
fn round3_conditional_concrete_branches_stay_in_core() {
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
    std::fs::write(
        directory.path().join("arguments.crn"),
        "arguments { flag: Bool }\n",
    )
    .unwrap();
    let source = "mock.test.Collection { params = if flag { 1 } else { 2 } }\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert!(
        diagnostics
            .iter()
            .all(|d| d.severity == Some(DiagnosticSeverity::ERROR)),
        "{diagnostics:#?}"
    );
}

#[test]
fn round3_expansion_merges_substituted_actual_types() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::string(),
        "let copies = for x in [1, false] {\n  mock.test.Collection { params = x }\n}\n",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
}

#[test]
fn round3_expansion_merges_substituted_map_keys_and_preserves_operands() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::map(AttributeType::string()),
        "let copies = for x in [{ first = 1 }, { second = false }] {\n  mock.test.Collection { params = x }\n}\n",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    let diagnostics = round2_source_diagnostics(
        AttributeType::map(AttributeType::string()),
        "let x = { first = 1 }\nlet y = { second = false }\nmock.test.Collection { params = x ?? y }\n",
    );
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
}

#[test]
fn round3_secret_interpolation_redacts_every_diagnostic() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Secret")
            .attribute(AttributeSchema::optional("text", AttributeType::string()).build())
            .attribute(AttributeSchema::optional("params", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    let engine = DiagnosticEngine::new(loaded_test_providers(schemas), vec!["mock".into()]);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let source = "let producer = mock.test.Secret {}\nmock.test.Secret { params = secret(\"private-${producer.text}\") }\n";
    let path = directory.path().join("main.crn");
    std::fs::write(&path, source).unwrap();
    let diagnostics = engine.analyze(&create_document_at(source, path.clone()), Some(&path));
    assert!(!diagnostics.is_empty());
    for diagnostic in &diagnostics {
        for rendered in [
            diagnostic.message.clone(),
            format!("{diagnostic:?}"),
            serde_json::to_string(diagnostic).unwrap(),
        ] {
            assert!(!rendered.contains("private-"), "{rendered}");
        }
    }
}

#[test]
fn round2_for_binding_outcomes_follow_parser_instance() {
    for expression in ["x", "[x]"] {
        let leaf = AttributeType::refined_string(None, None, Some((None, Some(1))), None);
        let field_type = if expression == "x" {
            leaf
        } else {
            AttributeType::list(leaf)
        };
        for items in [r#"["a", "abcd"]"#, r#"["abcd", "a"]"#] {
            let source = format!(
                "let copies = for x in {items} {{\n  mock.test.Collection {{ params = {expression} }}\n}}\n"
            );
            let diagnostics = round2_source_diagnostics(field_type.clone(), &source);
            assert_eq!(diagnostics.len(), 1, "{source}: {diagnostics:#?}");
            assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
            assert!(
                diagnostics[0].message.contains("length 4"),
                "{diagnostics:#?}"
            );
        }
    }
}

#[test]
fn round2_secret_concrete_leaf_has_only_core_error() {
    for expression in ["secret(42)", "secret(secret(42))"] {
        let diagnostics = collection_diagnostics(AttributeType::string(), expression);
        assert_eq!(diagnostics.len(), 1, "{expression}: {diagnostics:#?}");
        assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
        assert!(diagnostics[0].message.contains("expected String, got Int"));
    }
}

#[test]
fn round2_container_binding_enum_has_no_scalar_quick_fix() {
    use carina_core::schema::TypeIdentity;
    let enum_type = AttributeType::enum_(
        TypeIdentity::new(Some("mock"), ["test"], "Mode"),
        Some(vec!["ok".into()]),
        vec![],
        None,
        None,
    );
    let diagnostics = round2_source_diagnostics(
        AttributeType::list(enum_type.clone()),
        "let xs = [\"bad\"]\nmock.test.Collection { params = xs }\n",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(diagnostics[0].data.is_none(), "{diagnostics:#?}");
    let scalar = round2_source_diagnostics(
        enum_type,
        "let x = \"bad\"\nmock.test.Collection { params = x }\n",
    );
    assert_eq!(scalar.len(), 1, "{scalar:#?}");
    assert!(scalar[0].data.is_some(), "{scalar:#?}");
}

#[test]
fn round2_expansion_deduplicates_rule_with_substituted_values() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::refined_string(None, None, Some((None, Some(1))), None),
        "let copies = for x in [\"ab\", \"cd\"] {\n  mock.test.Collection { params = \"${x}\" }\n}\n",
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(diagnostics[0].message.contains("length 2"));
}

#[test]
fn round2_expansion_retains_each_authored_coalesce_operand() {
    let diagnostics = round2_source_diagnostics(
        AttributeType::refined_string(None, None, Some((None, Some(1))), None),
        "let producer = mock.test.Collection {}\nlet copies = for x in [\"ab\", \"cdef\"] {\n  mock.test.Collection { params = (producer.params ?? \"${x}\") ?? \"${x}\" }\n}\n",
    );
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.severity == Some(DiagnosticSeverity::ERROR))
    );
}

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
    let prefix = if value.contains("producer") {
        "let producer = mock.test.Collection {}\n"
    } else {
        ""
    };
    let source = format!("{prefix}mock.test.Collection {{\n  params = {value}\n}}\n");
    round2_source_diagnostics(field_type, &source)
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
