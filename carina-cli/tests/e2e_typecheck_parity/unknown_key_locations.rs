use super::*;

#[test]
fn inaccessible_unknown_map_keys_keep_validate_json_and_lsp_alive() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.Thing")
            .attribute(
                AttributeSchema::optional(
                    "protected",
                    AttributeType::map(AttributeType::struct_("Entry", vec![])),
                )
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "aliased",
                    AttributeType::map_with_key(
                        AttributeType::enum_(
                            carina_core::schema::TypeIdentity::bare("Mode"),
                            Some(vec!["Allow".into()]),
                            vec![("Allow".into(), "allow".into())],
                            None,
                            None,
                        ),
                        AttributeType::struct_("Entry", vec![]),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let engine = engine_with_schemas(schemas.clone());
    let mut failures = Vec::new();
    for (case, source, expected, message) in [
        (
            "protected",
            "mock.test.Thing { protected = secret({ 'PRIVATE_KEY' = { typo = true } }) }",
            (18, 27),
            "Map value for key '(secret)': Unknown field '(secret)' in Entry",
        ),
        (
            "alias",
            "mock.test.Thing { aliased = { allow = { typo = true } } }",
            (40, 44),
            "Map value for key 'allow': Unknown field 'typo' in Entry",
        ),
    ] {
        let fixture = write_fixture(&[
            ("providers.crn", "provider mock {}\n"),
            ("sibling.crn", source),
        ]);
        let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
            &carina_core::paths::ResolvedPath::new(fixture.path()).unwrap(),
            carina_cli::wiring::LoadedProviderFactory::for_test(factories_for(&schemas)),
            HashMap::new(),
        );
        let lsp = lsp_diagnostics(&engine, &fixture, "sibling.crn");
        eprintln!("{case} validate --json collector: {}", cli.json);
        eprintln!("{case} LSP: {lsp:#?}");
        let errors = cli.json["errors"].as_array().unwrap();
        if errors.iter().any(|error| {
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("provenance"))
        }) || lsp.iter().any(|error| error.message.contains("provenance"))
            || !errors.iter().any(|error| {
                error["message"]
                    .as_str()
                    .is_some_and(|message| message.contains("Unknown field"))
            })
            || !lsp
                .iter()
                .any(|error| error.message.contains("Unknown field"))
        {
            failures.push(case);
        }
        assert_eq!(errors.len(), 1, "{case}: {}", cli.json);
        assert_eq!(lsp.len(), 1, "{case}: {lsp:#?}");
        assert_eq!(errors[0]["message"], message);
        assert_eq!(lsp[0].message, message);
        assert_eq!(
            errors[0]["location"]["range"],
            serde_json::json!({
                "start": {"line": 1, "column": expected.0 + 1},
                "end": {"line": 1, "column": expected.1 + 1},
            })
        );
        assert_eq!(
            lsp[0].range,
            Range::new(Position::new(0, expected.0), Position::new(0, expected.1))
        );
    }
    assert!(
        failures.is_empty(),
        "unknown map keys aborted validation: {failures:?}"
    );
}
