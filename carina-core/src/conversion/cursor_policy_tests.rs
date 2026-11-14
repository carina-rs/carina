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
                "a conversion cannot be the operand of another conversion",
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
fn conversion_unknown_destination_is_owned_by_the_ordinary_schema_finding() {
    let source = "mock.Unknown { target = 'ok' as mock.Name }";
    let error = check(source, &schema()).unwrap_err();
    assert!(conversion_error(&error).is_none(), "{error}");
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
    use crate::validation::admission::attributes::ResourceAttributeInputs;
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
            let mut report = validation.admit_resource(
                crate::validation::admission::attributes::ResourceAdmissionRequest {
                    resource,
                    parsed: judged.file(),
                    schema: resource.schema(&schemas).unwrap(),
                    attributes: &classified,
                    quoted: resource.quoted_string_attrs(),
                    inputs: &inputs,
                    bindings: &bindings,
                    session,
                    schemas: &schemas,
                },
            );
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
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        validation.admit_remaining_resources(
            &tree,
            judged,
            &bindings,
            session,
            crate::validation::admission::attributes::SlotPrechecks::Unreported,
        );
    });
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
            crate::validation::admission::attributes::SlotPrechecks::Unreported,
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
fn r16_report_only_slot_reads_recorded_destination_then_checks_the_source() {
    let catalog = super::tests::catalog();
    let mut schemas = schema();
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(AttributeSchema::optional("output", named()).build())
            .attribute(AttributeSchema::optional("count", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    for (operand, expected) in [("'ok'", 0), ("p.output", 0), ("p.count", 1)] {
        let parsed = crate::parser::parse_file(
        &format!("let p = mock.Source {{}}\nlet sink = mock.Sink {{ target = {operand} as mock.Name }}"),
        Path::new("b1.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree
            .resources()
            .find(|resource| resource.binding() == Some("sink"))
            .unwrap();
        let values = resource.attributes().into_owned();
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
            let report = validation.admit_resource(
                crate::validation::admission::attributes::ResourceAdmissionRequest {
                    resource,
                    parsed: judged.file(),
                    schema: resource.schema(&schemas).unwrap(),
                    attributes: &classified,
                    quoted: resource.quoted_string_attrs(),
                    inputs: &inputs,
                    bindings: &bindings,
                    session,
                    schemas: &schemas,
                },
            );
            assert_eq!(report.errors().count(), 0, "{report:?}");
            assert_eq!(report.symbolic_errors().count(), 0, "{report:?}");
            crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
                validation.admit_remaining_resources(
                    &tree,
                    judged,
                    &bindings,
                    session,
                    crate::validation::admission::attributes::SlotPrechecks::Unreported,
                );
            });
            let conversions = validation.complete().into_presented();
            assert_eq!(
                conversion_errors(&conversions).len(),
                expected,
                "{conversions:?}"
            );
            if expected == 1 {
                assert_eq!(
                    conversion_errors(&conversions)[0].kind(),
                    ConversionErrorKind::ScalarKind
                );
            }
            assert!(
                !crate::test_utils::render_presented_for_test(&conversions)
                    .contains("bug in carina")
            );
        });
    }
}

#[test]
fn r11_stale_projection_can_only_reuse_an_already_recorded_static_verdict() {
    use crate::validation::admission::attributes::ResourceAttributeInputs;
    let catalog = super::tests::catalog();
    let schemas = schema();
    for converted in [false, true] {
        let suffix = if converted { " as mock.Name" } else { "" };
        let parsed = crate::parser::parse_file(
            &format!("let sink = mock.Sink {{ target = 'ok'{suffix} }}"),
            Path::new("stale-source.crn"),
            &catalog,
        )
        .unwrap()
        .resolve_root_for_test();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let inputs = ResourceAttributeInputs::for_resource(resource, judged.report()).unwrap();
        let classified = inputs.classify(
            &crate::resource::knowledge::KnowledgeContext::validation(),
            |_, _| {},
        );
        for instantiate in [false, true] {
            let mut changed = judged.file().clone();
            if instantiate {
                changed.resources[0] = changed.resources[0].instantiate();
            } else {
                let values = changed.resources[0].attributes.clone();
                changed.resources[0]
                    .authored_slots
                    .replace_projections(&values);
            }
            let changed_tree = crate::parser::AnalysisTree::try_from_file(&changed).unwrap();
            let resource = changed_tree.resources().next().unwrap();
            let bindings =
                crate::binding_index::BindingIndex::from_parsed(&changed, &schemas, &catalog);
            let mut run: crate::validation::ConversionValidation =
                crate::validation::ConversionValidation::new(judged.report());
            crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
                let findings = run.admit_resource(
                    crate::validation::admission::attributes::ResourceAdmissionRequest {
                        resource,
                        parsed: &changed,
                        schema: resource.schema(&schemas).unwrap(),
                        attributes: &classified,
                        quoted: resource.quoted_string_attrs(),
                        inputs: &inputs,
                        bindings: &bindings,
                        session,
                        schemas: &schemas,
                    },
                );
                assert_eq!(
                    findings.errors().count(),
                    usize::from(!converted),
                    "converted={converted}, instance={instantiate}: {findings:?}"
                );
                assert_eq!(findings.symbolic_errors().count(), 0, "{findings:?}");
            });
            let presented = run.complete().into_presented();
            assert_eq!(conversion_errors(&presented).len(), 0, "{presented:?}");
        }
    }
}

#[test]
fn round5_unresolved_or_reshaped_containers_require_conversion_descent() {
    use crate::validation::admission::attributes::ResourceAttributeInputs;
    let catalog = super::tests::catalog();
    #[derive(Debug, Clone, Copy)]
    enum Projection {
        Pending,
        Failed,
        Scalar,
    }
    for map in [false, true] {
        for union in [false, true] {
            for projection in [Projection::Pending, Projection::Failed, Projection::Scalar] {
                let mut schemas = SchemaRegistry::new();
                let destination = if matches!(projection, Projection::Scalar) {
                    AttributeType::string()
                } else if map {
                    AttributeType::map(named())
                } else {
                    AttributeType::list(named())
                };
                let destination = if union {
                    AttributeType::union(vec![destination, AttributeType::string()])
                } else {
                    destination
                };
                schemas.insert(
                    "mock",
                    ResourceSchema::new("Sink")
                        .attribute(AttributeSchema::optional("target", destination).build())
                        .validate()
                        .unwrap(),
                );
                let expression = if map {
                    "{ item = 42 as mock.Name }"
                } else {
                    "[42 as mock.Name]"
                };
                let parsed = crate::parser::parse_file(
                    &format!("let sink = mock.Sink {{ target = {expression} }}"),
                    Path::new("pending-container.crn"),
                    &catalog,
                )
                .unwrap()
                .resolve_root_for_test();
                let configuration =
                    crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
                let judged = configuration.judged();
                let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
                let resource = tree.resources().next().unwrap();
                let value = match projection {
                    Projection::Pending => {
                        crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(
                            crate::resource::ApplyTimeUnknown::PendingReRead(
                                crate::resource::PendingReRead::not_found(),
                            ),
                        ))
                    }
                    Projection::Failed => crate::resource::Value::Deferred(
                        crate::resource::DeferredValue::Unavailable {
                            name: crate::builtins::BuiltinFunction::env,
                            args: Vec::new(),
                            reason: crate::resource::UnavailableReason::from_builtin_error(
                                "ordinary evaluation failure".into(),
                            ),
                            source_line: crate::resource::DiagnosticSourceLine::synthetic(),
                        },
                    ),
                    Projection::Scalar => crate::resource::Value::Concrete(
                        crate::resource::ConcreteValue::String("ok".into()),
                    ),
                };
                let values = indexmap::IndexMap::from([("target".into(), value)]);
                let inputs =
                    ResourceAttributeInputs::from_resolved(resource, &values, judged.report())
                        .unwrap();
                let classified = inputs.classify(
                    &crate::resource::knowledge::KnowledgeContext::validation(),
                    |_, _| {},
                );
                assert!(match projection {
                    Projection::Pending => matches!(
                        classified["target"],
                        crate::resource::knowledge::Classification::Pending(_)
                    ),
                    Projection::Failed => matches!(
                        classified["target"],
                        crate::resource::knowledge::Classification::Failed(_)
                    ),
                    Projection::Scalar => matches!(
                        classified["target"],
                        crate::resource::knowledge::Classification::Known(_)
                    ),
                });
                let bindings = crate::binding_index::BindingIndex::from_parsed(
                    judged.file(),
                    &schemas,
                    &catalog,
                );
                let mut run: crate::validation::ConversionValidation =
                    crate::validation::ConversionValidation::new(judged.report());
                crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
                    let findings = run.admit_resource(
                        crate::validation::admission::attributes::ResourceAdmissionRequest {
                            resource,
                            parsed: judged.file(),
                            schema: resource.schema(&schemas).unwrap(),
                            attributes: &classified,
                            quoted: resource.quoted_string_attrs(),
                            inputs: &inputs,
                            bindings: &bindings,
                            session,
                            schemas: &schemas,
                        },
                    );
                    assert_eq!(
                        findings.errors().count(),
                        usize::from(!matches!(projection, Projection::Pending)),
                        "{projection:?}: {findings:?}"
                    );
                    assert_eq!(findings.symbolic_errors().count(), 0, "{findings:?}");
                });
                let presented = run.complete().into_presented();
                let conversions = conversion_errors(&presented);
                assert_eq!(
                    conversions.len(),
                    usize::from(!matches!(projection, Projection::Scalar)),
                    "map={map}, union={union}, {projection:?}: {presented:?}"
                );
                if let Some(conversion) = conversions.first() {
                    assert_eq!(conversion.kind(), ConversionErrorKind::ScalarKind);
                }
            }
        }
    }
}

fn r7_struct_destination() -> AttributeType {
    AttributeType::struct_(
        "B",
        vec![crate::schema::StructField::required("b", named()).build()],
    )
}
fn r7_union_destination() -> AttributeType {
    AttributeType::union(vec![
        AttributeType::struct_(
            "A",
            vec![crate::schema::StructField::required("a", AttributeType::string()).build()],
        ),
        r7_struct_destination(),
    ])
}

fn r7_pending_syntax(
    expression: &str,
    destination: AttributeType,
) -> crate::validation::PresentedFindings {
    r9_syntax_with_values(expression, destination, R9Projection::Pending)
}

#[derive(Clone, Copy)]
enum R9Projection {
    Pending,
    Absent,
}

fn r9_syntax_with_values(
    expression: &str,
    destination: AttributeType,
    projection: R9Projection,
) -> crate::validation::PresentedFindings {
    let values = match projection {
        R9Projection::Pending => indexmap::IndexMap::from([(
            "target".into(),
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::PendingReRead(
                    crate::resource::PendingReRead::not_found(),
                ),
            )),
        )]),
        R9Projection::Absent => Default::default(),
    };
    let catalog = super::tests::catalog();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("target", destination).build())
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_file(
        &format!("mock.Sink {{ target = {expression} }}"),
        Path::new("pending-union.crn"),
        &catalog,
    )
    .unwrap()
    .resolve_root_for_test();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let inputs = crate::validation::admission::attributes::ResourceAttributeInputs::from_resolved(
        resource,
        &values,
        judged.report(),
    )
    .unwrap();
    let classified = inputs.classify(
        &crate::resource::knowledge::KnowledgeContext::validation(),
        |_, _| {},
    );
    match projection {
        R9Projection::Pending => assert!(matches!(
            classified["target"],
            crate::resource::knowledge::Classification::Pending(_)
        )),
        R9Projection::Absent => assert!(classified.is_empty()),
    }
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let mut run: crate::validation::ConversionValidation =
        crate::validation::ConversionValidation::new(judged.report());
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let findings = run.admit_resource(
            crate::validation::admission::attributes::ResourceAdmissionRequest {
                resource,
                parsed: judged.file(),
                schema: resource.schema(&schemas).unwrap(),
                attributes: &classified,
                quoted: resource.quoted_string_attrs(),
                inputs: &inputs,
                bindings: &bindings,
                session,
                schemas: &schemas,
            },
        );
        assert_eq!(findings.errors().count(), 0, "{findings:?}");
    });
    run.complete().into_presented()
}

#[test]
fn r6_pending_union_cannot_choose_a_member_that_skips_the_conversion() {
    let presented = r7_pending_syntax("{ b = 'ok' as mock.Other }", r7_union_destination());
    let conversions = conversion_errors(&presented);
    assert_eq!(conversions.len(), 1, "{presented:?}");
    assert_eq!(conversions[0].kind(), ConversionErrorKind::Destination);
    assert!(
        !conversions[0].diagnostic_message().contains("not declared"),
        "{presented:?}"
    );
}

#[test]
fn r7_pending_union_accepts_the_member_that_judges_the_conversion() {
    let presented = r7_pending_syntax("{ b = 'ok' as mock.Name }", r7_union_destination());
    assert!(presented.is_empty(), "{presented:?}");
}

#[test]
fn r7_pending_struct_decline_names_the_undeclared_field() {
    let presented = r7_pending_syntax("{ unknown = 'ok' as mock.Name }", r7_struct_destination());
    let conversions = conversion_errors(&presented);
    assert_eq!(conversions.len(), 1, "{presented:?}");
    let message = conversions[0].diagnostic_message();
    assert!(
        message.contains("field 'unknown'") && message.contains("not declared by the destination"),
        "{message}"
    );
    assert_eq!(
        conversions[0].location().span().start_column(),
        "mock.Sink { target = { unknown = ".len() + 1
    );
}

#[test]
fn r7_pending_struct_decline_names_the_failed_assignment() {
    let destination = AttributeType::struct_(
        "Locked",
        vec![crate::schema::StructField::provider_populated("b", named()).build()],
    );
    let presented = r7_pending_syntax("{ b = 'ok' as mock.Name }", destination);
    let conversions = conversion_errors(&presented);
    assert_eq!(conversions.len(), 1, "{presented:?}");
    let message = conversions[0].diagnostic_message();
    let ordinary = crate::schema::TypeError::ReadOnlyAttribute { name: "b".into() }.to_string();
    assert!(
        message.contains("cannot be assigned") && message.contains(&ordinary),
        "{message}"
    );
}

#[test]
fn r7_declined_child_rejects_each_carried_convert_site() {
    let presented = r7_pending_syntax(
        "{ unknown = ['ok' as mock.Name, 'fine' as mock.Name] }",
        r7_struct_destination(),
    );
    let conversions = conversion_errors(&presented);
    assert_eq!(conversions.len(), 2, "{presented:?}");
    assert!(
        conversions
            .iter()
            .all(|error| error.diagnostic_message().contains("field 'unknown'")),
        "{presented:?}"
    );
}

#[test]
fn r7_no_as_pending_struct_declines_keep_mains_validation() {
    let presented = r7_pending_syntax("{ unknown = 'ok' }", r7_struct_destination());
    assert!(presented.is_empty(), "{presented:?}");
    let destination = AttributeType::struct_(
        "Locked",
        vec![crate::schema::StructField::provider_populated("b", named()).build()],
    );
    let presented = r7_pending_syntax("{ b = 'ok' }", destination);
    assert!(presented.is_empty(), "{presented:?}");
}

#[test]
fn r7_discarded_struct_decline_names_the_reason_and_preserves_the_no_as_twin() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("target", r7_struct_destination()).build())
            .validate()
            .unwrap(),
    );
    for (field, destination) in [
        ("unknown", r7_struct_destination()),
        (
            "b",
            AttributeType::struct_(
                "Locked",
                vec![crate::schema::StructField::provider_populated("b", named()).build()],
            ),
        ),
    ] {
        let mut schemas = schemas.clone();
        // The locked case has a separate writable selected arm field.
        let destination = if field == "b" {
            AttributeType::struct_(
                "Mixed",
                vec![
                    crate::schema::StructField::provider_populated("b", named()).build(),
                    crate::schema::StructField::optional("selected", named()).build(),
                ],
            )
        } else {
            destination
        };
        schemas.insert(
            "mock",
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::optional("target", destination).build())
                .validate()
                .unwrap(),
        );
        let selected = if field == "b" { "selected" } else { "b" };
        let source = format!(
            "let cond = false\nmock.Sink {{ target = if cond {{ {{ {field} = 'ok' as mock.Name }} }} else {{ {{ {selected} = 'ok' as mock.Name }} }} }}"
        );
        let error = check(&source, &schemas).unwrap_err();
        let crate::validation::ResourceRefTypeValidationError::Validation(presented) = error else {
            panic!("typed findings required");
        };
        let conversions = conversion_errors(&presented);
        assert_eq!(conversions.len(), 1, "{source}: {presented:?}");
        let reason = if field == "b" {
            "cannot be assigned"
        } else {
            "not declared by the destination"
        };
        assert!(
            conversions[0].diagnostic_message().contains(reason),
            "{presented:?}"
        );
        assert!(
            check(&source.replace(" as mock.Name", ""), &schemas).is_ok(),
            "no-as discarded fields retain main's behaviour"
        );
    }
}

#[test]
fn r5b_pending_transport_requires_a_shape_compatible_destination() {
    use crate::validation::admission::attributes::ResourceAttributeInputs;
    let catalog = super::tests::catalog();
    for expression in [
        "['ok']",
        "{ item = 'ok' }",
        "['ok' as mock.Name]",
        "{ item = 'ok' as mock.Name }",
    ] {
        let mut schemas = SchemaRegistry::new();
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
        let parsed = crate::parser::parse_file(
            &format!("mock.Sink {{ target = {expression} }}"),
            Path::new("shape.crn"),
            &catalog,
        )
        .unwrap()
        .resolve_root_for_test();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let values = indexmap::IndexMap::from([(
            "target".into(),
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::PendingReRead(
                    crate::resource::PendingReRead::not_found(),
                ),
            )),
        )]);
        let inputs =
            ResourceAttributeInputs::from_resolved(resource, &values, judged.report()).unwrap();
        let classified = inputs.classify(
            &crate::resource::knowledge::KnowledgeContext::validation(),
            |_, _| {},
        );
        let bindings =
            crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
        let mut run: crate::validation::ConversionValidation =
            crate::validation::ConversionValidation::new(judged.report());
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            let findings = run.admit_resource(
                crate::validation::admission::attributes::ResourceAdmissionRequest {
                    resource,
                    parsed: judged.file(),
                    schema: resource.schema(&schemas).unwrap(),
                    attributes: &classified,
                    quoted: resource.quoted_string_attrs(),
                    inputs: &inputs,
                    bindings: &bindings,
                    session,
                    schemas: &schemas,
                },
            );
            assert_eq!(
                findings.errors().count(),
                0,
                "main leaves the value pending; only carrying transports require a child destination: {findings:?}"
            );
        });
        let presented = run.complete().into_presented();
        assert_eq!(
            conversion_errors(&presented).len(),
            usize::from(expression.contains(" as ")),
            "{presented:?}"
        );
        if let Some(error) = conversion_errors(&presented).first() {
            assert!(
                error
                    .diagnostic_message()
                    .contains("conversion cannot be checked here")
            );
        }
    }
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
                "{}: {conversions:?}",
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
                        "{}: {conversions:?}",
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
fn r11_alias_uses_each_read_their_recorded_static_verdict() {
    use crate::validation::admission::attributes::ResourceAttributeInputs;
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
                let values = resource.attributes().into_owned();
                let inputs = if resource.binding() == Some("second") {
                    ResourceAttributeInputs::from_report(resource, &values, judged.report())
                } else {
                    ResourceAttributeInputs::for_resource(resource, judged.report()).unwrap()
                };
                let classified = inputs.classify(
                    &crate::resource::knowledge::KnowledgeContext::validation(),
                    |_, _| {},
                );
                let report = validation.admit_resource(
                    crate::validation::admission::attributes::ResourceAdmissionRequest {
                        resource,
                        parsed: judged.file(),
                        schema: resource.schema(&schemas).unwrap(),
                        attributes: &classified,
                        quoted: resource.quoted_string_attrs(),
                        inputs: &inputs,
                        bindings: &bindings,
                        session,
                        schemas: &schemas,
                    },
                );
                assert_eq!(report.errors().count(), 0, "{report:?}");
                assert_eq!(report.symbolic_errors().count(), 0, "{report:?}");
            }
        });
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            validation.admit_remaining_resources(
                &tree,
                judged,
                &bindings,
                session,
                crate::validation::admission::attributes::SlotPrechecks::Unreported,
            );
        });
        let conversions = validation.complete().into_presented();
        assert_eq!(conversions.len(), 0, "{source}\n{conversions:?}");
        assert_eq!(conversion_errors(&conversions).len(), 0, "{conversions:?}");
        assert!(
            !crate::test_utils::render_presented_for_test(&conversions).contains("bug in carina")
        );
    }
}

#[path = "review_round2_tests.rs"]
mod review_round2_tests;

#[test]
fn round5_unentered_syntax_does_not_require_a_conversion_verdict() {
    use crate::validation::ConversionValidation;
    let catalog = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse_file(
        "let converted = 'ok' as mock.Name\nmock.Sink { target = converted }",
        Path::new("not_admitted.crn"),
        &catalog,
    )
    .unwrap();
    let report = ConversionValidation::<std::convert::Infallible>::new(parsed.expression_report())
        .complete()
        .into_presented();
    assert!(report.is_empty(), "{report:?}");
    let plain = crate::parser::parse_file(
        "let ordinary = 'ok'",
        Path::new("not_admitted.crn"),
        &catalog,
    )
    .unwrap();
    assert!(
        ConversionValidation::<std::convert::Infallible>::new(plain.expression_report())
            .complete()
            .into_presented()
            .is_empty()
    );
}

#[test]
fn round5_partial_driver_retains_only_the_judged_slot_verdict() {
    for operand in ["'ok'", "42"] {
        let catalog = super::tests::catalog();
        let schemas = schema();
        let parsed = crate::parser::parse_file(
            &format!("mock.Sink {{ target = {operand} as mock.Name }}"),
            Path::new("partial.crn"),
            &catalog,
        )
        .unwrap();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let bindings =
            crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
        let mut run: crate::validation::ConversionValidation =
            crate::validation::ConversionValidation::new(judged.report());
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            let mut pass = run.admitted_resources(
                &tree,
                judged,
                &bindings,
                session,
                crate::validation::FindingSubject::Root,
            );
            assert!(pass.next().is_some());
            // Drop before exhaustion, even though this fixture has one resource.
        });
        let presented = run.complete().into_presented();
        assert_eq!(
            conversion_errors(&presented).len(),
            usize::from(operand == "42"),
            "{presented:?}"
        );
        assert!(
            !crate::test_utils::render_presented_for_test(&presented).contains("bug in carina"),
            "{presented:?}"
        );
    }
}

#[test]
fn round5_an_unentered_document_has_no_sink_findings() {
    let catalog = super::tests::catalog();
    let schemas = schema();
    let parsed = crate::parser::parse_file(
        "let converted = 'ok' as mock.Name\nmock.Sink { target = converted }",
        Path::new("not_admitted.crn"),
        &catalog,
    )
    .unwrap();
    let other = crate::parser::parse_file(
        "mock.Sink { target = 'ok' }",
        Path::new("other.crn"),
        &catalog,
    )
    .unwrap();
    let configuration = crate::config_loader::load_buffer_configuration(&other, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let mut run: crate::validation::ConversionValidation =
        crate::validation::ConversionValidation::new(parsed.expression_report());
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        run.admitted_resources(
            &tree,
            judged,
            &bindings,
            session,
            crate::validation::FindingSubject::Root,
        )
        .for_each(drop);
    });
    let presented = run.complete().into_presented();
    assert!(conversion_errors(&presented).is_empty(), "{presented:?}");
    assert!(presented.is_empty(), "{presented:?}");
}

#[test]
fn r5b_discarded_containers_keep_kind_only_judgements_without_a_live_child_destination() {
    let mut schemas = SchemaRegistry::new();
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
    for (operand, accepted) in [
        ("'ok'", true),
        ("'far too long for the member bound'", true),
        ("42", false),
    ] {
        for expression in [
            format!("[{operand} as mock.Name]"),
            format!("{{ item = {operand} as mock.Name }}"),
        ] {
            let source = format!(
                "mock.Sink {{ target = if true {{ 'ok' as mock.Name }} else {{ {expression} }} }}"
            );
            let result = check(&source, &schemas);
            if accepted {
                result.unwrap();
            } else {
                let error = result.unwrap_err();
                let conversion = conversion_error(&error)
                    .expect("the discarded Convert still needs its kind verdict");
                assert_eq!(
                    conversion.kind(),
                    ConversionErrorKind::ScalarKind,
                    "{error:?}"
                );
            }
        }
    }
}

#[test]
fn r6_secret_conversion_masks_typed_cause_debug() {
    use std::error::Error;
    const PLAINTEXT: &str = "PLAINTEXT-bad value";
    for source in [
        format!("mock.Sink {{ target = secret('{PLAINTEXT}') as mock.Name }}"),
        format!("let s = secret('{PLAINTEXT}')\nmock.Sink {{ target = s as mock.Name }}"),
    ] {
        let error = check(&source, &schema()).unwrap_err();
        let conversion = conversion_error(&error).unwrap();
        assert_eq!(conversion.kind(), ConversionErrorKind::Destination);
        assert!(!format!("{error:?}").contains(PLAINTEXT));
        assert!(!format!("{conversion:?}").contains(PLAINTEXT));
        assert!(
            conversion.source().is_some(),
            "the masked cause stays typed"
        );
        let mut cause: Option<&(dyn Error + 'static)> = Some(conversion);
        while let Some(error) = cause {
            assert!(!format!("{error} {error:?}").contains(PLAINTEXT));
            cause = error.source();
        }
    }
}

#[test]
fn r7_no_as_opaque_pairing_keeps_main_validation_then_plan_field_error() {
    use crate::resource::{
        ConcreteValue, Value,
        knowledge::{KnowledgeContext, PlanResolved},
    };
    let destination = AttributeType::struct_(
        "B",
        vec![crate::schema::StructField::optional("b", named()).build()],
    );
    let presented = r7_pending_syntax("{ unknown = 'ok' }", destination.clone());
    assert!(presented.is_empty(), "{presented:?}");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("target", destination).build())
            .validate()
            .unwrap(),
    );
    let value: Value = Value::Concrete(ConcreteValue::Map(indexmap::IndexMap::from([(
        "unknown".into(),
        Value::Concrete(ConcreteValue::String("ok".into())),
    )])));
    let resolved = crate::binding_index::ResolvedBindings::default();
    let proof =
        PlanResolved::try_from(value.knowledge(&KnowledgeContext::plan(&resolved)).unwrap())
            .unwrap();
    let attributes = indexmap::IndexMap::from([("target".into(), proof)]);
    let id = crate::resource::ResourceId::pending_with_provider("mock", "Sink", None);
    let catalog = super::tests::catalog();
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let findings = crate::validation::admission::attributes::admit_plan_attributes::<
            crate::validation::admission::kind::ResourceAttribute,
        >(
            schemas
                .get("mock", "Sink", crate::schema::SchemaKind::Resource)
                .unwrap(),
            &id,
            &attributes,
            &Default::default(),
            session,
        );
        let errors = findings.errors().collect::<Vec<_>>();
        assert_eq!(errors.len(), 1, "{findings:?}");
        assert!(
            matches!(errors[0], crate::schema::TypeError::UnknownStructField { field, .. } if field == "unknown"),
            "{findings:?}"
        );
    });
}

#[test]
fn r11_omitted_value_does_not_add_p3_after_a_judge_verdict() {
    for expression in ["'ok'", "'ok' as mock.Name", "42 as mock.Name"] {
        let presented = r9_syntax_with_values(expression, named(), R9Projection::Absent);
        let conversions = conversion_errors(&presented);
        let expected = usize::from(expression.starts_with("42"));
        assert_eq!(conversions.len(), expected, "{presented:?}");
        if let Some(error) = conversions.first() {
            assert_eq!(error.kind(), ConversionErrorKind::ScalarKind);
        }
        assert_eq!(presented.len(), expected, "{presented:?}");
    }
}

#[test]
fn r10_pending_nested_shape_validation_matches_the_no_as_control() {
    let mut results = Vec::new();
    for converted in [false, true] {
        let expression = if converted {
            "[['x' as mock.Name]]"
        } else {
            "[['x']]"
        };
        let catalog = super::tests::catalog();
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Sink")
                .attribute(
                    AttributeSchema::optional("target", AttributeType::list(named())).build(),
                )
                .validate()
                .unwrap(),
        );
        let parsed = crate::parser::parse_file(
            &format!("mock.Sink {{ target = {expression} }}"),
            Path::new("r10-pending.crn"),
            &catalog,
        )
        .unwrap()
        .resolve_root_for_test();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let values = indexmap::IndexMap::from([(
            "target".into(),
            crate::resource::Value::Deferred(crate::resource::DeferredValue::Unknown(
                crate::resource::ApplyTimeUnknown::PendingReRead(
                    crate::resource::PendingReRead::not_found(),
                ),
            )),
        )]);
        let inputs =
            crate::validation::admission::attributes::ResourceAttributeInputs::from_resolved(
                resource,
                &values,
                judged.report(),
            )
            .unwrap();
        let classified = inputs.classify(
            &crate::resource::knowledge::KnowledgeContext::validation(),
            |_, _| {},
        );
        assert!(matches!(
            classified["target"],
            crate::resource::knowledge::Classification::Pending(_)
        ));
        let bindings =
            crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
        let mut run: crate::validation::ConversionValidation =
            crate::validation::ConversionValidation::new(judged.report());
        let ordinary =
            crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
                let result = run.admit_resource(
                    crate::validation::admission::attributes::ResourceAdmissionRequest {
                        resource,
                        parsed: judged.file(),
                        schema: resource.schema(&schemas).unwrap(),
                        attributes: &classified,
                        quoted: resource.quoted_string_attrs(),
                        inputs: &inputs,
                        bindings: &bindings,
                        session,
                        schemas: &schemas,
                    },
                );
                result.errors().map(ToString::to_string).collect::<Vec<_>>()
            });
        let presented = run.complete().into_presented();
        println!(
            "R10-PENDING converted={converted}: source={expression}; judge={:?}; ordinary={ordinary:?}; presented={presented:?}",
            judged.report().findings()
        );
        results.push((
            converted,
            !ordinary.is_empty() || !presented.is_empty() || !judged.report().findings().is_empty(),
        ));
    }
    // The projected Pending value postpones ordinary shape validation for
    // both forms; this probe does not establish an as/no-as difference.
    assert_eq!(results, vec![(false, false), (true, false)]);
    let value = crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(vec![
        crate::resource::Value::Concrete(crate::resource::ConcreteValue::List(vec![
            crate::resource::Value::Concrete(crate::resource::ConcreteValue::String("x".into())),
        ])),
    ]));
    let catalog = super::tests::catalog();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("target", AttributeType::list(named())).build())
            .validate()
            .unwrap(),
    );
    let resolved = crate::binding_index::ResolvedBindings::default();
    let proof = crate::resource::knowledge::PlanResolved::try_from(
        value
            .knowledge(&crate::resource::knowledge::KnowledgeContext::plan(
                &resolved,
            ))
            .unwrap(),
    )
    .unwrap();
    let attributes = indexmap::IndexMap::from([("target".into(), proof)]);
    let id = crate::resource::ResourceId::pending_with_provider("mock", "Sink", None);
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let findings = crate::validation::admission::attributes::admit_plan_attributes::<
            crate::validation::admission::kind::ResourceAttribute,
        >(
            schemas
                .get("mock", "Sink", crate::schema::SchemaKind::Resource)
                .unwrap(),
            &id,
            &attributes,
            &Default::default(),
            session,
        );
        let errors = findings
            .errors()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        println!("R10-PENDING operational_plan={errors:?}");
        assert_eq!(errors.len(), 1, "{findings:?}");
        assert!(
            errors[0].contains("expected mock.Name, got List"),
            "{errors:?}"
        );
    });
}

#[test]
fn r11_judge_owned_static_conversion_failures_do_not_require_admission() {
    let catalog = super::tests::catalog();
    let mut results = Vec::new();
    for (expression, destination, kind) in [
        ("'ok' as Int", named(), ConversionErrorKind::Target),
        (
            "'ok' as mock.Other",
            named(),
            ConversionErrorKind::Destination,
        ),
        ("7 as mock.Name", named(), ConversionErrorKind::ScalarKind),
        (
            "['ok'] as mock.Name",
            named(),
            ConversionErrorKind::NotScalar,
        ),
        (
            "'ok' as mock.Name",
            AttributeType::union(vec![named(), named()]),
            ConversionErrorKind::AmbiguousDestination,
        ),
    ] {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Sink")
                .attribute(AttributeSchema::optional("target", destination).build())
                .validate()
                .unwrap(),
        );
        let source = format!("mock.Sink {{ target = {expression} }}");
        let parsed = crate::parser::parse_file(&source, Path::new("judge-owned.crn"), &catalog)
            .unwrap()
            .resolve_root_for_test();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        // No resource admission walk: read-only and stopped runs still own
        // exactly main's Judge findings before a provider may be dispatched.
        let presented = crate::validation::ConversionValidation::<std::convert::Infallible>::new(
            configuration.judged().report(),
        )
        .complete()
        .into_presented();
        let errors = conversion_errors(&presented);
        println!("R11-JUDGE source={source}; findings={presented:?}");
        results.push((source, errors.len() == 1 && errors[0].kind() == kind));
    }
    assert!(
        results.iter().all(|(_, correct)| *correct),
        "static verdict missing without resource admission: {results:?}"
    );
}

#[test]
fn r11_walk_consumes_a_judge_verdict_without_resolving_the_target_again() {
    use crate::parser::retained::{AuthoredCursor, conversion::check as policy};
    use crate::schema::value_validation::{AuthoredOutcome, CheckValue};
    use crate::validation::admission::kind::ResourceAttribute;
    let catalog = super::tests::catalog();
    let schemas = schema();
    for (operand, accepted) in [("'ok'", true), ("42", false)] {
        let parsed = crate::parser::parse_file(
            &format!("mock.Sink {{ target = {operand} as mock.Name }}"),
            Path::new("static-replay.crn"),
            &catalog,
        )
        .unwrap()
        .resolve_root_for_test();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let destination = resource.schema(&schemas).unwrap();
        let declaration = destination
            .schema()
            .as_schema()
            .attributes()
            .get("target")
            .unwrap()
            .parts()
            .attr_type;
        let cursor =
            AuthoredCursor::authored(resource.authored_slot("target").unwrap().cursor().unwrap());
        let values = resource.attributes();
        // This catalog cannot resolve mock.Name. A recomputation would either
        // reject the accepted case or emit a second target finding.
        let empty = crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let verdict = policy::<ResourceAttribute, _, _>(
            declaration,
            CheckValue::Current(&values["target"]),
            &crate::schema::LeafPath::from_segments([]),
            cursor.clone(),
            destination.schema().as_schema().defs(),
            crate::parser::retained::ConversionContext {
                catalog: &empty,
                reference: crate::validation::admission::ReferenceSource::Unchecked,
            },
        )
        .unwrap();
        let witness = match verdict {
            AuthoredOutcome::Claimed(witness) => witness,
            AuthoredOutcome::Continue(_, _) | AuthoredOutcome::Transport(_) => {
                panic!("the Convert needs a verdict")
            }
            AuthoredOutcome::Deferred(never) => match never {},
        };
        let result = witness.consume(declaration, &cursor).unwrap().decide(
            |findings, _owner| (true, findings.conversions.findings().len()),
            |findings| (false, findings.conversions.findings().len()),
        );
        assert_eq!(result, (accepted, 0));
        let presented = crate::validation::ConversionValidation::<std::convert::Infallible>::new(
            judged.report(),
        )
        .complete()
        .into_presented();
        assert_eq!(conversion_errors(&presented).len(), usize::from(!accepted));
    }
}
