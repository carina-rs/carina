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
        crate::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let _tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
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
                let (): () = run
                    .admit_remaining_resources(
                        judged,
                        &bindings,
                        session,
                        crate::validation::admission::attributes::SlotPrechecks::Unreported,
                    )
                    .unwrap();
            }
            assert!(run.complete().into_presented().is_empty());
        });
        diagnostics.push(format!("{:?}", result.diagnostics()));
    }
    assert_eq!(diagnostics[0], diagnostics[1]);
    assert!(diagnostics[0].contains("existing shared check"));
}

#[test]
fn r5b_no_as_sibling_driver_does_no_extra_walks() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.crn"),
        "let producer = mock.Source {}\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("b.crn"),
        "mock.Sink { target = 'ok' plain = producer.output }\n",
    )
    .unwrap();
    let catalog = crate::conversion::tests::catalog();
    let schemas = schema();
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(directory.path()).unwrap(),
        &catalog,
    )
    .unwrap()
    .into_parsed()
    .resolve_root_for_test();
    let configuration =
        crate::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let _tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let mut run: crate::validation::ConversionValidation =
            crate::validation::ConversionValidation::new(judged.report());
        let before = run.walk_in(crate::validation::FindingSubject::Root, |coverage| {
            coverage.work()
        });
        run.admit_remaining_resources(
            judged,
            &bindings,
            session,
            crate::validation::admission::attributes::SlotPrechecks::Unreported,
        )
        .unwrap();
        let after = run.walk_in(crate::validation::FindingSubject::Root, |coverage| {
            coverage.work()
        });
        assert_eq!(
            after, before,
            "a no-as directory must do zero sibling admission walks"
        );
    });
}

#[test]
fn r5b_sibling_driver_does_not_repeat_a_completed_conversion_validator() {
    use crate::schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity,
    };
    use crate::validation::admission::attributes::{
        ResourceAdmissionRequest, ResourceAttributeInputs,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::refined_string_with_validator(
                        TypeIdentity::from_dotted("mock.Name"),
                        None,
                        None,
                        crate::schema::validator(move |_| {
                            counter.fetch_add(1, Ordering::SeqCst);
                            Ok(())
                        }),
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let catalog = crate::conversion::tests::catalog();
    let parsed = crate::parser::parse_file(
        "mock.Sink { target = 'ok' as mock.Name }",
        Path::new("carrying-sibling.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let configuration =
        crate::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let inputs = ResourceAttributeInputs::for_resource_for_test(resource, judged.report()).unwrap();
    let mut run: crate::validation::ConversionValidation =
        crate::validation::ConversionValidation::new(judged.report());
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let (report, _prefixes_for_test) = run
            .admit_resource(ResourceAdmissionRequest::for_test(
                resource,
                judged.file(),
                resource.schema(&schemas).unwrap(),
                resource.quoted_string_attrs(),
                inputs,
                crate::validation::admission::attributes::ValidationEvaluation::None,
                &bindings,
                session,
                &schemas,
            ))
            .values
            .into_parts();
        assert_eq!(report.errors().count(), 0, "{report:?}");
        let after = calls.load(Ordering::SeqCst);
        assert!(
            after > 0,
            "this must exercise the destination's native validator"
        );
        let work = run.walk_in(crate::validation::FindingSubject::Root, |coverage| {
            coverage.work()
        });
        run.admit_remaining_resources(
            judged,
            &bindings,
            session,
            crate::validation::admission::attributes::SlotPrechecks::Unreported,
        )
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            after,
            "diagnostic dedup cannot undo a repeated provider callback"
        );
        assert_eq!(
            run.walk_in(crate::validation::FindingSubject::Root, |coverage| coverage
                .work()),
            work
        );
    });
    assert!(run.complete().into_presented().is_empty());
}
