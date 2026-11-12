use super::tests::{conversion_error, conversion_errors, ordinary_messages};
use super::*;
use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeIdentity};
use std::{collections::HashSet, path::Path};

#[test]
fn conversion_chains_have_one_outer_diagnostic() {
    for depth in [2, 3] {
        let source = format!(
            "mock.Sink {{ target = 'ok'{} }}",
            " as mock.Name".repeat(depth)
        );
        let error = check(&source, &schema()).unwrap_err();
        let crate::validation::ResourceRefTypeValidationError::Validation(report) = error else {
            panic!("typed conversion required: {error:?}");
        };
        let findings = conversion_errors(&report);
        assert_eq!(findings.as_slice().len(), 1, "{depth}: {findings:?}");
        let error = &findings.as_slice()[0];
        assert!(
            error
                .diagnostic_message()
                .contains("cannot be the operand of another conversion"),
            "{error}"
        );
        assert_eq!(
            error.location().span().start_column(),
            source.find("'ok'").unwrap() + 1
        );
        assert_eq!(
            error.location().span().end_column(),
            source.rfind(" }").unwrap() + 1
        );
    }
}

#[test]
fn conversion_chains_keep_outer_owner_under_existing_restrictions() {
    for depth in [2, 3] {
        let chain = format!("'ok'{}", " as mock.Name".repeat(depth));
        for (expression, message) in [
            (
                format!("secret({chain})"),
                "checked conversion failed: (secret) failed validation",
            ),
            (
                format!("\"${{{chain}}}\""),
                "conversion is not supported inside this expression",
            ),
        ] {
            let source = format!("mock.Sink {{ target = {expression} }}");
            let error = check(&source, &schema()).unwrap_err();
            let crate::validation::ResourceRefTypeValidationError::Validation(report) = error
            else {
                panic!("typed findings required: {error:?}");
            };
            let errors = conversion_errors(&report);
            assert_eq!(errors.len(), 1, "{source}: {errors:?}");
            assert_eq!(errors[0].diagnostic_message(), message);
            assert_eq!(
                errors[0].location().span().start_column(),
                source.find("'ok'").unwrap() + 1
            );
            assert_eq!(
                errors[0].location().span().end_column(),
                source.rfind("mock.Name").unwrap() + "mock.Name".len() + 1
            );
        }
    }
}

#[test]
fn conversion_without_resource_schema_reports_unsupported_position() {
    let source = "mock.Unknown { target = 'ok' as mock.Name }";
    let error = check(source, &schema()).unwrap_err();
    let conversion = conversion_error(&error).unwrap();
    assert_eq!(conversion.kind(), ConversionErrorKind::UnsupportedPosition);
    assert_eq!(
        conversion.diagnostic_message(),
        "conversion is not supported at this position"
    );
    let crate::validation::ResourceRefTypeValidationError::Validation(report) = error else {
        panic!("typed findings report required");
    };
    assert_eq!(
        ordinary_messages(&report),
        "Unknown resource type: mock.Unknown"
    );
}

#[test]
fn conversion_channel_is_disjoint_from_ordinary_findings_across_resource_slots() {
    use crate::validation::admission::attributes::{
        ResourceAttributeInputs, resource_attribute_admissions,
    };
    let schemas = schema();
    let catalog = super::tests::catalog();
    let source = "let converted = 42 as mock.Name\nlet first = mock.Sink { target = converted }\nlet second = mock.Sink { target = converted }";
    let parsed = crate::parser::parse_file(source, Path::new("channels.crn"), &catalog)
        .unwrap()
        .resolve_root_for_test();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let mut validation: crate::validation::ConversionValidation =
        crate::validation::ConversionValidation::new(judged.report());
    let mut resources = 0;
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        for resource in tree.resources() {
            let inputs = ResourceAttributeInputs::for_resource(resource, judged.report()).unwrap();
            let classified = inputs.classify(
                &crate::resource::knowledge::KnowledgeContext::validation(),
                |_, _| {},
            );
            let mut report = validation.walk(|coverage| {
                resource_attribute_admissions(
                    resource,
                    judged.file(),
                    resource.schema(&schemas).unwrap(),
                    &classified,
                    resource.quoted_string_attrs(),
                    &inputs,
                    &bindings,
                    session,
                    &schemas,
                    coverage,
                )
            });
            assert_eq!(report.errors().count(), 0, "conversion leaked into errors");
            assert_eq!(
                report.symbolic_errors().count(),
                0,
                "conversion leaked into symbolic errors"
            );
            assert_eq!(
                report.diagnostic_errors().count(),
                0,
                "conversion leaked into diagnostic errors"
            );
            assert!(
                report
                    .take_attribute("target")
                    .unwrap()
                    .symbolic_findings()
                    .is_empty()
            );
            resources += 1;
        }
    });
    assert_eq!(resources, 2);
    let conversions = validation.complete().into_presented();
    assert_eq!(conversion_errors(&conversions).len(), 1);
    assert_eq!(
        conversion_errors(&conversions)[0].kind(),
        ConversionErrorKind::ScalarKind
    );
}

#[test]
fn conversion_rejected_mixed_parent_skips_destination_value_checks() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for expression in [
        "if true { 'ok' as mock.Name } else { 'ok' }",
        "('ok' as mock.Name) ?? 'ok'",
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
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
                                count.fetch_add(1, Ordering::SeqCst);
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
        let error = check(&format!("mock.Sink {{ target = {expression} }}"), &schemas).unwrap_err();
        assert_eq!(
            conversion_error(&error).unwrap().kind(),
            ConversionErrorKind::UnsupportedPosition
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{expression}");
    }
}

#[test]
fn conversion_unrendered_coverage_does_not_repeat_native_map_key_or_value_checks() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let keys = Arc::new(AtomicUsize::new(0));
    let values = Arc::new(AtomicUsize::new(0));
    let native = |name: &str, count: Arc<AtomicUsize>| {
        AttributeType::refined_string_with_validator(
            TypeIdentity::from_dotted(name),
            None,
            None,
            crate::schema::validator(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            None,
        )
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::map_with_key(
                        native("mock.Key", keys.clone()),
                        native("mock.Value", values.clone()),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_file(
        "mock.Sink { target = { ok = 'ok' } }",
        Path::new("sibling.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        crate::validation::admission::attributes::unrendered_resource_conversion_admissions(
            resource,
            judged,
            &bindings,
            session,
            &mut Default::default(),
        );
    });
    assert_eq!(
        keys.load(Ordering::SeqCst),
        0,
        "unrendered ordinary keys keep main's callback count"
    );
    assert_eq!(
        values.load(Ordering::SeqCst),
        0,
        "unrendered ordinary values keep main's callback count"
    );
}

fn schema() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("target", named()).build())
            .attribute(AttributeSchema::optional("plain", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    schemas
}
fn named() -> AttributeType {
    AttributeType::refined_string(
        Some(TypeIdentity::from_dotted("mock.Name")),
        None,
        Some((Some(1), Some(8))),
        None,
    )
}
fn check(
    source: &str,
    schemas: &SchemaRegistry,
) -> Result<(), crate::validation::ResourceRefTypeValidationError> {
    let catalog = super::tests::catalog();
    let parsed = crate::parser::parse_file(source, Path::new("b1.crn"), &catalog)
        .unwrap()
        .resolve_root_for_test();
    let configuration = crate::config_loader::load_buffer_configuration(&parsed, schemas, &catalog);
    let judged = configuration.judged();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), schemas, &catalog);
    crate::validation::validate_resource_admissions(
        judged,
        &bindings,
        &HashSet::from(["mock".into()]),
        &mut Vec::new(),
    )
}

#[test]
fn conversion_boundaries_are_typed_b2_placeholders() {
    for source in [
        "provider mock { endpoint = 'ok' as mock.Name }",
        "backend local { path = 'ok' as mock.Name }",
        "exports { name = 'ok' as mock.Name }",
        "attributes { name = 'ok' as mock.Name }",
        "fn supply(): String { 'ok' as mock.Name }",
        "fn supply(value: String = 'ok' as mock.Name) { value }",
    ] {
        let error = check(source, &schema())
            .err()
            .unwrap_or_else(|| panic!("boundary admitted: {source}"));
        assert_eq!(
            conversion_error(&error).unwrap().kind(),
            ConversionErrorKind::UnsupportedPosition,
            "{source}: {error:?}"
        );
    }
}

#[test]
fn conversion_boundary_alias_use_takes_precedence_over_unused_definition() {
    for source in [
        "let converted = 'ok' as mock.Name\nexports { name = converted }",
        "let converted = 'ok' as mock.Name\nlet alias = converted\nexports { name = alias }",
        "let converted = 'ok' as mock.Name\nprovider mock { endpoint = converted }",
    ] {
        let error = check(source, &schema()).unwrap_err();
        let crate::validation::ResourceRefTypeValidationError::Validation(report) = error else {
            panic!("typed conversion finding required");
        };
        let findings = conversion_errors(&report);
        assert_eq!(findings.as_slice().len(), 1, "{source}: {findings:?}");
        assert_eq!(
            findings.as_slice()[0].kind(),
            ConversionErrorKind::UnsupportedPosition,
            "{source}: {findings:?}"
        );
    }
}

#[test]
fn conversion_claim_does_not_skip_ordinary_siblings_or_unknown_fields() {
    for field in ["plain = 'wrong kind'", "unknown = 42"] {
        let source = format!("mock.Sink {{ target = 'ok' as mock.Name {field} }}");
        let error = check(&source, &schema()).unwrap_err();
        assert!(conversion_error(&error).is_none(), "{error:?}");
        assert!(
            error.to_string().contains(if field.starts_with("plain") {
                "Int"
            } else {
                "unknown"
            }),
            "{error}"
        );
    }
}

#[test]
fn conversion_wrappers_do_not_hide_secret_or_conversion_operand_placement() {
    for expression in [
        "secret(('ok' as mock.Name))",
        "(('ok' as mock.Name)) as mock.Name",
    ] {
        let source = format!("mock.Sink {{ target = {expression} }}");
        let error = check(&source, &schema()).unwrap_err();
        assert_eq!(
            conversion_error(&error).unwrap().kind(),
            ConversionErrorKind::UnsupportedPosition,
            "{error:?}"
        );
    }
}

#[test]
fn conversion_discarded_union_rejects_ambiguous_identity() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional("target", AttributeType::union(vec![named(), named()]))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let error = check(
        "mock.Sink { target = if false { 'ok' as mock.Name } else { 'ok' as mock.Name } }",
        &schemas,
    )
    .unwrap_err();
    assert_eq!(
        conversion_error(&error).unwrap().kind(),
        ConversionErrorKind::AmbiguousDestination
    );
    let crate::validation::ResourceRefTypeValidationError::Validation(report) = error else {
        panic!("structured conversion errors required");
    };
    let findings = conversion_errors(&report);
    assert_eq!(
        findings.as_slice().len(),
        2,
        "current and discarded conversions each need the union declaration"
    );
}

#[test]
fn conversion_no_authored_evidence_cannot_admit_a_converted_slot() {
    let catalog = super::tests::catalog();
    let schemas = schema();
    let parsed = crate::parser::parse_file(
        "let sink = mock.Sink { target = 'ok' as mock.Name }",
        Path::new("b1.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let values = resource.resolved_attributes();
    let inputs = crate::validation::admission::attributes::ResourceAttributeInputs::from_report(
        resource,
        &values,
        judged.report(),
    );
    let classified = inputs.classify(
        &crate::resource::knowledge::KnowledgeContext::validation(),
        |_, _| {},
    );
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let mut validation: crate::validation::ConversionValidation =
            crate::validation::ConversionValidation::new(judged.report());
        let report = validation.walk(|coverage| {
            crate::validation::admission::attributes::resource_attribute_admissions(
                resource,
                judged.file(),
                resource.schema(&schemas).unwrap(),
                &classified,
                resource.quoted_string_attrs(),
                &inputs,
                &bindings,
                session,
                &schemas,
                coverage,
            )
        });
        assert_eq!(report.errors().count(), 0, "{report:?}");
        assert_eq!(report.symbolic_errors().count(), 0, "{report:?}");
        let conversions = validation.complete().into_presented();
        assert_eq!(conversion_errors(&conversions).len(), 1);
        assert_eq!(
            conversion_errors(&conversions)[0].kind(),
            ConversionErrorKind::UnsupportedPosition
        );
    });
}

#[test]
fn conversion_pending_mixed_branches_are_parent_owned_in_both_orders() {
    for expression in [
        "if choose { converted } else { 'ok' }",
        "if choose { 'ok' } else { converted }",
    ] {
        let source = format!(
            "arguments {{ choose: Bool }}\nlet converted = 'ok' as mock.Name\nmock.Sink {{ target = {expression} }}"
        );
        let error = check(&source, &schema()).unwrap_err();
        let conversion = conversion_error(&error).unwrap();
        assert_eq!(conversion.kind(), ConversionErrorKind::UnsupportedPosition);
        assert_eq!(conversion.location().span().start_line(), 3);
    }
}

#[test]
fn conversion_borrows_the_resolved_ref_declaration() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .with_def("Target", named())
            .attribute(AttributeSchema::optional("target", AttributeType::ref_("Target")).build())
            .validate()
            .unwrap(),
    );
    check("mock.Sink { target = 'ok' as mock.Name }", &schemas).unwrap();
}

#[test]
fn conversion_union_ref_members_resolve_identity_and_bounds() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .with_def("Target", named())
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::union(vec![
                        AttributeType::ref_("Target"),
                        AttributeType::string(),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    check("mock.Sink { target = 'ok' as mock.Name }", &schemas).unwrap();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(
                AttributeSchema::optional(
                    "output",
                    AttributeType::refined_string(
                        Some(TypeIdentity::from_dotted("foreign.Name")),
                        None,
                        Some((Some(9), Some(16))),
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let error = check(
        "let producer = mock.Source {}\nmock.Sink { target = producer.output as mock.Name }",
        &schemas,
    )
    .unwrap_err();
    assert_eq!(
        conversion_error(&error).unwrap().kind(),
        ConversionErrorKind::UnsupportedPosition
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .with_def("Target", named())
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::union(vec![
                        AttributeType::ref_("Target"),
                        AttributeType::ref_("Target"),
                    ]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    for expression in [
        "'ok' as mock.Name",
        "if false { 'ok' as mock.Name } else { 'ok' as mock.Name }",
    ] {
        let error = check(&format!("mock.Sink {{ target = {expression} }}"), &schemas).unwrap_err();
        assert_eq!(
            conversion_error(&error).unwrap().kind(),
            ConversionErrorKind::AmbiguousDestination
        );
    }
}

#[test]
fn conversion_union_failures_keep_matching_member_cause_and_skip_rejected_value_checks() {
    use std::{
        error::Error,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::union(vec![AttributeType::string(), named()]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let error = check(
        "mock.Sink { target = 'toolong-for-slot' as mock.Name }",
        &schemas,
    )
    .unwrap_err();
    let conversion = conversion_error(&error).unwrap();
    assert!(
        conversion.source().is_some_and(|cause| cause
            .downcast_ref::<crate::schema::TypeError>()
            .is_some_and(|error| matches!(
                error,
                crate::schema::TypeError::LengthOutOfRange { .. }
            ))),
        "the matching member owns the value failure: {error:?}"
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let member = AttributeType::refined_string_with_validator(
        TypeIdentity::from_dotted("mock.Name"),
        None,
        None,
        crate::schema::validator(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }),
        None,
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "target",
                    AttributeType::union(vec![member.clone(), member]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let error = check("mock.Sink { target = 'ok' as mock.Name }", &schemas).unwrap_err();
    assert_eq!(
        conversion_error(&error).unwrap().kind(),
        ConversionErrorKind::AmbiguousDestination
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "a statically rejected scalar conversion cannot reach destination value checks"
    );
}

#[test]
fn conversion_known_destination_failure_retains_its_cause_and_convert_range() {
    use std::error::Error;
    let source = "mock.Sink { target = 'toolong-for-slot' as mock.Name }";
    let error = check(source, &schema()).unwrap_err();
    let conversion =
        conversion_error(&error).expect("destination rejection needs the Convert owner");
    assert_eq!(conversion.kind(), ConversionErrorKind::Destination);
    assert_eq!(
        conversion.location().span().start_column(),
        source.find("'toolong").unwrap() + 1
    );
    assert!(conversion.source().is_some_and(|cause| {
        cause
            .downcast_ref::<crate::schema::TypeError>()
            .is_some_and(|error| matches!(error, crate::schema::TypeError::LengthOutOfRange { .. }))
    }));
}

#[test]
fn conversion_covered_mixed_branches_report_the_convert_once() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("target", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let source = "mock.Sink { target = \"${if true { 'ok' as mock.Name } else { 'ok' }}\" }";
    let error = check(source, &schemas).unwrap_err();
    let crate::validation::ResourceRefTypeValidationError::Validation(report) = error else {
        panic!("typed conversion finding required");
    };
    let findings = conversion_errors(&report);
    assert_eq!(findings.as_slice().len(), 1, "{findings:?}");
    assert_eq!(
        findings.as_slice()[0].location().span().start_column(),
        source.find("'ok' as").unwrap() + 1
    );
}

#[test]
fn conversion_no_as_directory_inventory_is_vacuous_without_any_walk_evidence() {
    let cases: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../tests/fixtures/conversion_no_as_parity.json"
    ))
    .unwrap();
    let catalog = super::tests::catalog();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::optional("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    for case in cases {
        let directory = tempfile::tempdir().unwrap();
        for (file, source) in case["files"].as_object().unwrap() {
            let path = directory.path().join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source.as_str().unwrap()).unwrap();
        }
        let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let parsed = crate::config_loader::parse_directory(&path, &catalog).unwrap();
        {
            let configuration = crate::config_loader::load_buffer_configuration(
                parsed.parsed(),
                &schemas,
                &catalog,
            );
            let conversions =
                crate::validation::ConversionValidation::<std::convert::Infallible>::new(
                    configuration.judged().report(),
                )
                .complete()
                .into_presented();
            assert!(
                conversion_errors(&conversions).is_empty(),
                "{}: {conversions}",
                case["name"]
            );
        }
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            match crate::module_resolver::resolve_modules_with_providerless_config(
                parsed, &path, session, &schemas,
            ) {
                Ok(graph) => {
                    assert_eq!(case["expected"]["status"], "ok", "{}", case["name"]);
                    let configuration =
                        crate::config_loader::load_buffer_configuration(&graph, &schemas, &catalog);
                    let conversions = crate::validation::ConversionValidation::<
                        std::convert::Infallible,
                    >::new(configuration.judged().report())
                    .complete()
                    .into_presented();
                    assert!(
                        conversion_errors(&conversions).is_empty(),
                        "{}: {conversions}",
                        case["name"]
                    );
                }
                Err(error) => {
                    assert_eq!(
                        case["expected"]["status"], "error",
                        "{}: {error}",
                        case["name"]
                    );
                    assert!(
                        error
                            .to_string()
                            .contains(case["expected"]["errors"][0]["message"].as_str().unwrap()),
                        "{error}"
                    );
                }
            }
        });
    }
}

#[test]
fn conversion_one_authored_alias_use_cannot_cover_another_use_without_evidence() {
    use crate::validation::admission::attributes::{
        ResourceAttributeInputs, resource_attribute_admissions,
    };
    let schemas = schema();
    let catalog = super::tests::catalog();
    for second in ["converted", "'ok'"] {
        let source = format!(
            "let converted = 'ok' as mock.Name\nlet first = mock.Sink {{ target = converted }}\nlet second = mock.Sink {{ target = {second} }}"
        );
        let parsed = crate::parser::parse_file(&source, Path::new("alias-evidence.crn"), &catalog)
            .unwrap()
            .resolve_root_for_test();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let bindings =
            crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
        let mut validation: crate::validation::ConversionValidation =
            crate::validation::ConversionValidation::new(judged.report());
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            for resource in tree.resources() {
                let values = resource.resolved_attributes();
                let inputs = if resource.binding() == Some("second") {
                    ResourceAttributeInputs::from_report(resource, &values, judged.report())
                } else {
                    ResourceAttributeInputs::for_resource(resource, judged.report()).unwrap()
                };
                let classified = inputs.classify(
                    &crate::resource::knowledge::KnowledgeContext::validation(),
                    |_, _| {},
                );
                let report = validation.walk(|coverage| {
                    resource_attribute_admissions(
                        resource,
                        judged.file(),
                        resource.schema(&schemas).unwrap(),
                        &classified,
                        resource.quoted_string_attrs(),
                        &inputs,
                        &bindings,
                        session,
                        &schemas,
                        coverage,
                    )
                });
                assert_eq!(report.errors().count(), 0, "{report:?}");
                assert_eq!(report.symbolic_errors().count(), 0, "{report:?}");
            }
        });
        let conversions = validation.complete().into_presented();
        assert_eq!(
            conversion_errors(&conversions).len(),
            usize::from(second == "converted"),
            "{source}\n{conversions}"
        );
        for error in conversion_errors(&conversions) {
            assert_eq!(error.kind(), ConversionErrorKind::UnsupportedPosition);
        }
    }
}

#[path = "review_round2_tests.rs"]
mod review_round2_tests;
