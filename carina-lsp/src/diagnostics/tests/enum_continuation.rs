use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
};

#[test]
fn substituted_enum_from_sibling_keeps_main_diagnostic_list() {
    let root = tempfile::tempdir().unwrap();
    let main = "mock.EnumSink { value = a.name }\n";
    for (name, body) in [
        ("main.crn", main),
        ("sources.crn", "let a = mock.Source { name = 'bad' }\n"),
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ] {
        std::fs::write(root.path().join(name), body).unwrap();
    }
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
    let engine =
        super::DiagnosticEngine::new(super::loaded_test_providers(schemas), vec!["mock".into()]);
    let path = root.path().join("main.crn");
    let doc = super::create_document_at(main, root.path().join("main.crn"));
    let diagnostics = engine.analyze(&doc, Some(&path));
    // Measured on origin/main f7ac5a3fe: this reference mismatch owns the
    // diagnostic; the substituted invalid member adds no enum diagnostic.
    assert_eq!(
        diagnostics,
        vec![super::Diagnostic {
            range: super::Range::new(super::Position::new(0, 16), super::Position::new(0, 21)),
            severity: Some(super::DiagnosticSeverity::WARNING),
            source: Some("carina".into()),
            message: "Type mismatch: expected mock.Choice, got String (from a.name)".into(),
            ..Default::default()
        }]
    );
    for name in ["sources.crn", "providers.crn", "backend.crn"] {
        let path = root.path().join(name);
        let doc = super::create_document_at(&std::fs::read_to_string(&path).unwrap(), path.clone());
        assert!(engine.analyze(&doc, Some(&path)).is_empty(), "{name}");
    }
}
