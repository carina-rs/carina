use super::*;

#[test]
fn conversion_rejected_struct_union_has_no_explained_ordinary_mismatch() {
    use crate::schema::StructField;
    for (second, expected_ordinary) in [("'ok'", 0), ("42", 1)] {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Sink")
                .attribute(
                    AttributeSchema::optional(
                        "target",
                        AttributeType::union(vec![
                            AttributeType::struct_(
                                "Record",
                                vec![
                                    StructField::required("a", named()).build(),
                                    StructField::required("b", AttributeType::string()).build(),
                                ],
                            ),
                            AttributeType::string(),
                        ]),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        let source = format!("mock.Sink {{ target = {{ a = 42 as mock.Name b = {second} }} }}");
        let crate::validation::ResourceRefTypeValidationError::Validation(report) =
            check(&source, &schemas).unwrap_err()
        else {
            panic!("typed report")
        };
        assert_eq!(conversion_errors(&report).len(), 1, "{report:?}");
        assert_eq!(
            ordinary_messages(&report).lines().count(),
            expected_ordinary,
            "{report:?}"
        );
        if expected_ordinary != 0 {
            assert!(ordinary_messages(&report).contains("b"), "{report:?}");
        }
    }
}

#[test]
fn conversion_deferred_union_is_an_unsupported_position_until_persistence() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::union(vec![named(), AttributeType::string()]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let source =
        "let producer = mock.Source {}\nmock.Sink { target = producer.output as mock.Name }";
    let error =
        check(source, &schemas).expect_err("a deferred selected member is not persisted by B1");
    let crate::validation::ResourceRefTypeValidationError::Validation(report) = error else {
        panic!("typed report")
    };
    assert!(ordinary_messages(&report).is_empty(), "{report:?}");
    assert_eq!(conversion_errors(&report).len(), 1, "{report:?}");
    assert_eq!(
        conversion_errors(&report)[0].kind(),
        ConversionErrorKind::UnsupportedPosition
    );
    assert!(
        conversion_errors(&report)[0]
            .diagnostic_message()
            .contains("not supported yet")
    );
    check("mock.Sink { target = 'ok' as mock.Name }", &schemas).unwrap();
}

#[test]
fn conversion_operand_failure_is_the_ordinary_main_error() {
    let catalog = super::super::tests::catalog();
    let ordinary = crate::parser::parse_file("let a = (1 / 0)", Path::new("operand.crn"), &catalog)
        .unwrap_err();
    let converted = crate::parser::parse_file(
        "let a = (1 / 0) as mock.Count",
        Path::new("operand.crn"),
        &catalog,
    )
    .unwrap_err();
    assert_eq!(converted.to_string(), ordinary.to_string());
}

#[test]
fn conversion_unrendered_no_as_walk_cannot_emit_or_add_shared_sink_findings() {
    use crate::validation::deferred_types::{ProviderTypeCheckSite, TypeCheckSession};
    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = schema();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let source = "let producer = mock.Source {}\nmock.Sink { target = producer.output plain = 'wrong ordinary type' }";
    let parsed = crate::parser::parse_file(source, Path::new("sibling.crn"), &catalog)
        .unwrap()
        .resolve_root_for_test();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let expected = crate::parser::parse_type_expr_str("missing.Type", &catalog).unwrap();
    let mut diagnostics = Vec::new();
    for walk_siblings in [false, true] {
        let result = TypeCheckSession::run(&catalog, |session| {
            session.record(
                &expected,
                ProviderTypeCheckSite::directory_scoped("existing shared check"),
            );
            let mut run: crate::validation::ConversionValidation =
                crate::validation::ConversionValidation::new(judged.report());
            if walk_siblings {
                // The production sibling driver has no ordinary findings output.
                let (): () = run.admit_remaining_resources(&tree, judged, &bindings, session);
            }
            assert!(run.complete().into_presented().is_empty());
        });
        diagnostics.push(format!("{:?}", result.diagnostics()));
    }
    assert_eq!(diagnostics[0], diagnostics[1]);
    assert!(diagnostics[0].contains("existing shared check"));
}
