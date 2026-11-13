use super::*;

#[test]
fn round5_placement_provenance_expansion_is_linear_across_computed_parents() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for count in [24, 48] {
        // Aliases keep parser nesting bounded while the contributing
        // judgement DAG still has the same skewed N/2N computed-parent chain.
        let mut source = "let value0 = 1 as unloaded.Name\n".to_string();
        for index in 1..count {
            source.push_str(&format!(
                "let value{index} = value{} + (1 as unloaded.Name)\n",
                index - 1
            ));
        }
        source.push_str(&format!("mock.Sink {{ target = value{} }}", count - 1));
        let parsed = super::super::parse_file(&source, Path::new("p1-work.crn"), &catalog).unwrap();
        // This is the producer's work/output, before the sole NodeId collector.
        // A repeated expansion must not build a quadratic error vector there.
        let produced = parsed.expression_report().placements.findings().len();
        eprintln!("P1 provenance: {count} Converts, {produced} producer records");
        assert!(
            produced <= 2 * count,
            "{count} Converts produced {produced} records"
        );
        let presented =
            ConversionValidation::<std::convert::Infallible>::new(parsed.expression_report())
                .complete()
                .into_presented();
        assert_eq!(
            crate::conversion::tests::conversion_errors(&presented).len(),
            count
        );
    }
}
use crate::validation::ConversionValidation;

#[test]
fn conversion_placement_findings_do_not_depend_on_report_node_maps() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for (source, count) in [
        ("provider mock { endpoint = 'ok' as String }", 1),
        ("provider mock { endpoint = 'ok' }", 0),
    ] {
        let parsed = super::super::parse_file(source, Path::new("missing.crn"), &catalog).unwrap();
        let mut report = parsed.expression_report().clone();
        Arc::make_mut(&mut report.nodes).clear();
        let schemas = crate::schema::SchemaRegistry::new();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = super::super::AnalysisTree::try_from_file(judged.file()).unwrap();
        let bindings =
            crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
        let mut validation = ConversionValidation::<std::convert::Infallible>::new(&report);
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            validation
                .admitted_resources(
                    &tree,
                    judged,
                    &bindings,
                    session,
                    crate::validation::FindingSubject::Root,
                )
                .for_each(drop);
        });
        let conversions = validation.complete().into_presented();
        assert_eq!(
            crate::conversion::tests::conversion_errors(&conversions).len(),
            count
        );
        for error in crate::conversion::tests::conversion_errors(&conversions) {
            assert_eq!(
                error.kind(),
                crate::ConversionErrorKind::UnsupportedPosition
            );
            assert_eq!(
                error.location().span().start_column(),
                source.find("'ok'").unwrap() + 1
            );
        }
    }
}

#[test]
fn round5_condition_roles_reject_an_aliased_conversion_as_computed() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for tail in [
        "arguments { input: Bool { validation { condition = converted error_message = 'bad' } } }",
        "arguments { input: Bool { validation { condition = !converted error_message = 'bad' } } }",
        "arguments { input: Bool { validation { condition = converted || false error_message = 'bad' } } }",
        "arguments { input: Bool { validation { condition = converted && true error_message = 'bad' } } }",
    ] {
        let source = format!("let converted = true as unloaded.Name\n{tail}");
        let parsed =
            super::super::parse_file(&source, Path::new("condition-alias.crn"), &catalog).unwrap();
        let presented =
            ConversionValidation::<std::convert::Infallible>::new(parsed.expression_report())
                .complete()
                .into_presented();
        let errors = crate::conversion::tests::conversion_errors(&presented);
        assert_eq!(errors.len(), 1, "{source}: {presented:?}");
        assert_eq!(
            errors[0].diagnostic_message(),
            "conversion is not supported at this position",
            "{source}"
        );
    }
}

#[test]
fn round5_map_keys_follow_alias_judgements() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for resource in [false, true] {
        for expanded in [true, false] {
            let value = if resource {
                "mock.Sink { \"${converted}\" = 'ok' }"
            } else {
                "mock.Sink { target = { \"${converted}\" = 'ok' } }"
            };
            let tail = if expanded {
                value.into()
            } else {
                format!("for item in [] {{ {value} }}")
            };
            let source = format!("let converted = 'key' as unloaded.Name\n{tail}");
            let parsed = super::super::parse_file(&source, Path::new("key-alias.crn"), &catalog);
            if expanded {
                let ordinary = parsed.unwrap_err();
                let twin = super::super::parse_file(
                    &source.replace(" as unloaded.Name", ""),
                    Path::new("key-alias.crn"),
                    &catalog,
                )
                .unwrap_err();
                assert_eq!(
                    ordinary.to_string(),
                    twin.to_string(),
                    "interpolated keys fail before Judge on main too"
                );
                continue;
            }
            let parsed = parsed.unwrap();
            let presented =
                ConversionValidation::<std::convert::Infallible>::new(parsed.expression_report())
                    .complete()
                    .into_presented();
            let errors = crate::conversion::tests::conversion_errors(&presented);
            assert_eq!(errors.len(), 1, "{presented:?}");
            assert_eq!(
                errors[0].kind(),
                crate::ConversionErrorKind::UnsupportedPosition
            );
        }
    }
}

#[test]
fn conversion_discarded_union_claim_is_judged_once_and_covers_wrapped_leaf() {
    use crate::schema::value_validation::{
        AssignmentScope, AuthoredChecks, RequiredAssignmentCallbacks,
    };
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional(
                    "input",
                    AttributeType::union(vec![AttributeType::string(), AttributeType::int()]),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = super::super::parse_file(
        "mock.Sink { input = (('ok' as mock.Name)) }",
        Path::new("convert.crn"),
        &catalog,
    )
    .unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = super::super::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let slot = resource.authored_slot("input").unwrap();
    let schema = resource.schema(&schemas).unwrap();
    let ordinary = std::cell::Cell::new(0);
    let conversions = std::cell::Cell::new(0);
    let findings = crate::schema::value_validation::visit_discarded(
        &schema.schema().as_schema().attributes()["input"].attr_type, schema.schema().as_schema().defs(),
        &crate::schema::FieldPath::new(), crate::schema::FieldAncestry::WRITABLE, slot.cursor().unwrap(),
        RequiredAssignmentCallbacks {
            ordinary: &|_, _, _, _, _| { ordinary.set(ordinary.get() + 1); Ok(Default::default()) },
            authored: AuthoredChecks::Validation(&|attr, checked, path, cursor| {
                if matches!(&cursor.kind, CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) if usage.form() == AuthoredForm::Convert) { conversions.set(conversions.get() + 1); }
                check_conversion::<crate::validation::admission::kind::ResourceAttribute, ParserPlaceholder, RelativeBindingName>(attr, checked, path, cursor, schema.schema().as_schema().defs(), &catalog)
            }), scope: AssignmentScope::Full,
        }, &mut Default::default(),
    ).unwrap();
    assert_eq!(conversions.get(), 1);
    assert_eq!(
        ordinary.get(),
        0,
        "conversion-carrying transports cannot mint an ordinary assignment proof"
    );
    assert!(findings.authored.errors.is_empty());
    assert_eq!(findings.authored.conversions.findings().len(), 1);
}

#[test]
fn conversion_does_not_reclassify_an_int_as_a_string_construction() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = super::super::parse_file(
        "let converted = 3 as mock.Count",
        Path::new("convert.crn"),
        &catalog,
    )
    .unwrap();
    let schemas = crate::schema::SchemaRegistry::new();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let syntax = judged
        .file()
        .parser_binding_metadata
        .syntax()
        .expressions()
        .binding(&RelativeBindingName::new("converted"))
        .unwrap();
    let expression = judged
        .report()
        .node(&ExpressionInstance::Root, syntax.node_id())
        .unwrap();
    let sink = TypeExpr::Int;
    let view = expression.for_sink(&sink, &catalog);
    assert!(!view.is_single_string_construction());
    assert_eq!(
        expression.binding_diagnostic_owner(Some(&sink), &sink, &catalog),
        BindingDiagnosticOwner::AuthoredExpression
    );
}

#[test]
fn conversion_chained_wrappers_each_keep_their_own_authored_range() {
    let source = "let value = 'ok' as mock.Name as mock.Other";
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = super::super::parse_file(source, Path::new("convert.crn"), &catalog).unwrap();
    let expression = parsed
        .parser_binding_metadata
        .syntax()
        .expressions()
        .binding(&RelativeBindingName::new("value"))
        .unwrap();
    let mut outer = expression.authored.as_ref();
    while matches!(outer.kind, ExprKind::Alias) {
        outer = &outer.children[0].authored;
    }
    let inner = &outer.children[0].authored;
    assert!(matches!(outer.kind, ExprKind::Convert { .. }));
    assert!(matches!(inner.kind, ExprKind::Convert { .. }));
    assert_ne!(inner.id, outer.id);
    assert_eq!(
        inner.location.declaration().span().end_column(),
        source.find(" as mock.Other").unwrap() + 1
    );
    assert_eq!(
        outer.location.declaration().span().end_column(),
        source.len() + 1
    );
}

#[test]
fn conversion_live_covered_and_discarded_uses_each_supply_a_verdict() {
    use crate::schema::value_validation::{
        AssignmentScope, AuthoredChecks, AuthoredOutcome, CheckValue, RequiredAssignmentCallbacks,
    };
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    for (expression, expected) in [
        ("'ok' as String", [1, 0, 0]),
        ("\"${'ok' as String}\"", [0, 1, 0]),
        (
            "if true { 'ok' as String } else { 'ok' as String }",
            [1, 0, 1],
        ),
    ] {
        let parsed = super::super::parse_file(
            &format!("mock.Sink {{ input = {expression} }}"),
            Path::new("convert.crn"),
            &catalog,
        )
        .unwrap();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = super::super::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let slot = resource.authored_slot("input").unwrap();
        let schema = resource.schema(&schemas).unwrap();
        let proof = slot
            .projection()
            .unwrap()
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let verdicts = std::cell::Cell::new([0; 3]);
        let mut validation: ConversionValidation = ConversionValidation::new(judged.report());
        let mut observed_coverage = crate::schema::value_validation::WalkCoverage::default();
        let result = {
            let coverage = &mut observed_coverage;
            crate::schema::value_validation::validate_authored_value_for_admission(
                &schema.schema().as_schema().attributes()["input"].attr_type,
                &proof,
                schema.schema().as_schema().defs(),
                crate::schema::FieldAncestry::WRITABLE,
                &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
                AuthoredCursor::authored(slot.cursor().unwrap()),
                RequiredAssignmentCallbacks {
                    ordinary: &|_, _, _, _, _| Ok(Default::default()),
                    authored: AuthoredChecks::Validation(&|attr, checked, path, cursor| {
                        let is_convert = match &cursor.kind {
                            CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) => {
                                usage.form() == AuthoredForm::Convert
                            }
                            CursorKind::Projected(_) => false,
                        };
                        let outcome = check_conversion::<
                            crate::validation::admission::kind::ResourceAttribute,
                            ParserPlaceholder,
                            RelativeBindingName,
                        >(
                            attr,
                            checked,
                            path,
                            cursor,
                            schema.schema().as_schema().defs(),
                            &catalog,
                        )?;
                        if is_convert {
                            assert!(
                                matches!(outcome, AuthoredOutcome::Claimed(_)),
                                "a Convert requires its own verdict"
                            );
                            let index = match checked {
                                CheckValue::Current(_) => 0,
                                CheckValue::Covered => 1,
                                CheckValue::Discarded => 2,
                                CheckValue::Pending => panic!(
                                    "fixture supplies current, covered or discarded evidence"
                                ),
                            };
                            let mut counts = verdicts.get();
                            counts[index] += 1;
                            verdicts.set(counts);
                        }
                        Ok(outcome)
                    }),
                    scope: AssignmentScope::Full,
                },
                coverage,
            )
        };
        let bindings =
            crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
        crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
            validation
                .admitted_resources(
                    &tree,
                    judged,
                    &bindings,
                    session,
                    crate::validation::FindingSubject::Root,
                )
                .for_each(drop);
        });
        let presented = validation.complete().into_presented();
        assert!(result.local_error().is_none());
        assert_eq!(
            crate::conversion::tests::conversion_errors(&presented).len(),
            expected.iter().sum::<usize>()
        );
        assert_eq!(verdicts.get(), expected, "{expression}");
    }
}

#[test]
fn conversion_claim_preserves_ordinary_checks_below_a_rejected_wrapper() {
    use crate::schema::value_validation::{
        AssignmentScope, AuthoredChecks, RequiredAssignmentCallbacks,
    };
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(
                AttributeSchema::optional("input", AttributeType::list(AttributeType::int()))
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = super::super::parse_file(
        "mock.Sink { input = [1, 2] as mock.Name }",
        Path::new("convert.crn"),
        &catalog,
    )
    .unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = super::super::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let slot = resource.authored_slot("input").unwrap();
    let schema = resource.schema(&schemas).unwrap();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let ordinary = std::cell::RefCell::new(Vec::new());
    let mut validation: ConversionValidation = ConversionValidation::new(judged.report());
    let mut observed_coverage = crate::schema::value_validation::WalkCoverage::default();
    let result = {
        let coverage = &mut observed_coverage;
        crate::schema::value_validation::validate_authored_value_for_admission(
            &schema.schema().as_schema().attributes()["input"].attr_type,
            &proof,
            schema.schema().as_schema().defs(),
            crate::schema::FieldAncestry::WRITABLE,
            &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
            AuthoredCursor::authored(slot.cursor().unwrap()),
            RequiredAssignmentCallbacks {
                ordinary: &|_, _, path, _, _| {
                    ordinary.borrow_mut().push(path.clone());
                    Ok(Default::default())
                },
                authored: AuthoredChecks::Validation(&|attr, checked, path, cursor| {
                    check_conversion::<
                        crate::validation::admission::kind::ResourceAttribute,
                        ParserPlaceholder,
                        RelativeBindingName,
                    >(
                        attr,
                        checked,
                        path,
                        cursor,
                        schema.schema().as_schema().defs(),
                        &catalog,
                    )
                }),
                scope: AssignmentScope::Full,
            },
            coverage,
        )
    };
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        validation
            .admitted_resources(
                &tree,
                judged,
                &bindings,
                session,
                crate::validation::FindingSubject::Root,
            )
            .for_each(drop);
    });
    let presented = validation.complete().into_presented();
    assert!(result.local_error().is_none());
    assert_eq!(
        crate::conversion::tests::conversion_errors(&presented).len(),
        1
    );
    let paths = ordinary.into_inner();
    assert_eq!(
        paths.len(),
        2,
        "the rejected wrapper owns only its current value: {paths:?}"
    );
    assert!(paths.iter().all(|path| !path.segments().is_empty()));
}

#[test]
fn conversion_cursor_wrapper_keeps_typed_path_and_operand_value() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = super::super::parse_file(
        "let converted = 'ok' as mock.Name",
        Path::new("convert.crn"),
        &catalog,
    )
    .unwrap();
    let expression = parsed
        .parser_binding_metadata
        .syntax()
        .expressions()
        .binding(&RelativeBindingName::new("converted"))
        .unwrap();
    assert_eq!(
        expression.projection(),
        Some(&Value::Concrete(crate::resource::ConcreteValue::String(
            "ok".into()
        )))
    );
    let mut node = expression.authored.as_ref();
    while matches!(node.kind, ExprKind::Alias) {
        node = &node.children[0].authored;
    }
    let ExprKind::Convert { target, .. } = &node.kind else {
        panic!("missing Convert");
    };
    assert_eq!(target.detail().as_str(), "mock.Name");
    assert_eq!(node.children.len(), 1);
}

#[test]
fn conversion_grammar_requires_a_type_path_and_keyword_boundary() {
    for text in [
        "'ok' as mock.Name",
        "'ok' as String",
        "'ok' as mock.Name as mock.Other",
    ] {
        assert!(
            super::super::parse_file(
                &format!("let x = {text}"),
                Path::new("convert.crn"),
                &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty())
            )
            .is_ok()
        );
    }
    for source in [
        "let x = 'ok' as list(String)",
        "let x = 'ok' as mock.1",
        "let x = 'ok' asdf",
    ] {
        assert!(
            super::super::parse_file(
                source,
                Path::new("convert.crn"),
                &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty())
            )
            .is_err()
        );
    }
}

#[test]
fn conversion_formatter_preserves_conversion_and_comments() {
    let source = "let converted='ok' as /* target */ mock.Name\n";
    let formatted =
        crate::formatter::format(source, &crate::formatter::FormatConfig::default()).unwrap();
    assert!(formatted.contains("as"), "{formatted}");
    assert!(formatted.contains("/* target */"), "{formatted}");
    assert!(formatted.contains("mock.Name"), "{formatted}");
    super::super::parse_file(
        &formatted,
        Path::new("convert.crn"),
        &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert_eq!(
        crate::formatter::format(&formatted, &crate::formatter::FormatConfig::default()).unwrap(),
        formatted
    );
}

#[test]
fn conversion_parser_failures_keep_unsupported_placement_and_the_convert_range() {
    for source in [
        "let value = { \"${42 as String}\" = 'ok' }",
        "let value = if 42 as String { 'ok' } else { 'ok' }",
        "let value = does_not_exist(42 as String)",
        "let value = ('ok' as String) + 1",
    ] {
        let parsed = super::super::parse_file(
            source,
            Path::new("convert.crn"),
            &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap();
        let presented =
            ConversionValidation::<std::convert::Infallible>::new(parsed.expression_report())
                .complete()
                .into_presented();
        let errors = crate::conversion::tests::conversion_errors(&presented);
        assert_eq!(errors.len(), 1, "{source}: {presented:?}");
        let conversion = errors[0];
        assert_eq!(
            conversion.kind(),
            crate::ConversionErrorKind::UnsupportedPosition,
            "{presented:?}"
        );
        assert!(conversion.location().span().start_column() > 1);
    }
}

#[test]
fn conversion_quoted_keys_own_the_convert_range_and_one_typed_error() {
    for source in [
        r#"mock.Sink { "${'target' as mock.Name}" = 'ok' }"#,
        r#"let value = { "${'key' as mock.Name}" = 'ok' }"#,
    ] {
        let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let parsed = super::super::parse_file(source, Path::new("keys.crn"), &catalog).unwrap();
        let presented =
            ConversionValidation::<std::convert::Infallible>::new(parsed.expression_report())
                .complete()
                .into_presented();
        let errors = crate::conversion::tests::conversion_errors(&presented);
        assert_eq!(errors.len(), 1);
        let conversion = errors[0];
        assert_eq!(
            conversion.kind(),
            crate::ConversionErrorKind::UnsupportedPosition
        );
        assert_eq!(
            conversion.location().span().start_column(),
            source.find("'").unwrap() + 1
        );
        assert_eq!(
            crate::test_utils::render_presented_for_test(&presented)
                .matches(&conversion.to_string())
                .count(),
            1
        );
    }
}

#[test]
fn round5_unclaimed_convert_cannot_reach_ordinary_value_admission() {
    use crate::schema::value_validation::{
        AssignmentScope, AuthoredChecks, AuthoredOutcome, RequiredAssignmentCallbacks,
    };
    use crate::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let parsed = super::super::parse_file(
        "mock.Sink { input = 'ok' as mock.Name }",
        Path::new("unguarded.crn"),
        &catalog,
    )
    .unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = super::super::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let slot = resource.authored_slot("input").unwrap();
    let schema = resource.schema(&schemas).unwrap();
    let proof = slot
        .projection()
        .unwrap()
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let mut coverage = crate::schema::value_validation::WalkCoverage::default();
    let findings = crate::schema::value_validation::validate_authored_value_for_admission(
        &schema.schema().as_schema().attributes()["input"].attr_type,
        &proof,
        schema.schema().as_schema().defs(),
        crate::schema::FieldAncestry::WRITABLE,
        &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
        AuthoredCursor::authored(slot.cursor().unwrap()),
        RequiredAssignmentCallbacks {
            ordinary: &|_, _, _, _, _| Ok(Default::default()),
            authored: AuthoredChecks::Validation(&|_, _, _, cursor| {
                Ok(AuthoredOutcome::Continue(
                    cursor
                        .no_conversion()
                        .unwrap_or_else(crate::test_utils::no_conversion_proof),
                    Default::default(),
                ))
            }),
            scope: AssignmentScope::Full,
        },
        &mut coverage,
    );
    assert_eq!(findings.local_errors().count(), 0, "{findings:?}");
    assert_eq!(
        coverage.take_conversions().findings().len(),
        1,
        "ordinary value acceptance must require a source proof"
    );
}

#[test]
fn r5b_loop_elements_and_slots_reject_without_a_parent_rejection() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for source in [
        "for n in [1 as mock.Count] { mock.Sink { target = n } }",
        "for (i, n) in [1 as mock.Count] { mock.Sink { target = i } }",
        "let data = { a = 1 as mock.Count }\nfor k, n in data { mock.Sink { target = k } }",
    ] {
        let parsed =
            super::super::parse_file(source, Path::new("element-role.crn"), &catalog).unwrap();
        let report = parsed.expression_report();
        let mut checked = 0;
        for node in report
            .syntax_nodes
            .values()
            .filter(|node| matches!(node.kind, ExprKind::Element | ExprKind::LoopSlot(_)))
        {
            let children = node
                .children
                .iter()
                .map(|child| {
                    report
                        .node(&child.authored.location.instance, child.node_id())
                        .unwrap()
                        .clone()
                })
                .collect::<Vec<_>>();
            let mut output = conversion::flow::PlacementFindings::new(Default::default());
            assert!(
                conversion::flow::check_placement(
                    node,
                    &children,
                    &conversion::flow::ConversionSources::empty(),
                    false,
                    None,
                    &mut output
                ),
                "{source}"
            );
            assert_eq!(output.into_findings().findings().len(), 1, "{source}");
            checked += 1;
        }
        assert!(
            checked > 0,
            "the grammar fixture must exercise the loop role: {source}"
        );
    }
}

#[test]
fn r5b_directive_attribute_and_block_are_placement_boundaries() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for directive in [
        "directives { depends_on = [x as mock.Name] }",
        "directives = { depends_on = [x as mock.Name] }",
    ] {
        let source = format!("let x = mock.Source {{}}\nmock.Sink {{ {directive} }}");
        let parsed =
            super::super::parse_file(&source, Path::new("directives.crn"), &catalog).unwrap();
        let presented =
            ConversionValidation::<std::convert::Infallible>::new(parsed.expression_report())
                .complete()
                .into_presented();
        let conversions = crate::conversion::tests::conversion_errors(&presented);
        assert_eq!(conversions.len(), 1, "{directive}: {presented:?}");
        assert_eq!(
            conversions[0].kind(),
            crate::ConversionErrorKind::UnsupportedPosition
        );
        assert_eq!(
            conversions[0].diagnostic_message(),
            "conversion is not supported at this position yet"
        );
    }
}

#[test]
fn r5b_unloaded_transport_destinations_decline_before_child_admission() {
    use crate::schema::value_validation::{
        AssignmentScope, AuthoredChecks, RequiredAssignmentCallbacks, WalkCoverage,
    };
    use crate::schema::{
        AttrTypeKind, AttributeSchema, AttributeType, CompiledAttributeType, ResourceSchema,
        SchemaRegistry, TypeIdentity,
    };
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let destination = CompiledAttributeType {
        kind: AttrTypeKind::UnloadedSchemaType(
            crate::parser::UnloadedSchemaType::try_from(TypeIdentity::bare("Unavailable")).unwrap(),
        ),
    };
    for (converted, expression) in [
        (false, "['ok']"),
        (true, "['ok' as unloaded.Name]"),
        (false, "{ item = 'ok' }"),
        (true, "{ item = 'ok' as unloaded.Name }"),
    ] {
        let parsed = super::super::parse_file(
            &format!("mock.Sink {{ input = {expression} }}"),
            Path::new("unloaded.crn"),
            &catalog,
        )
        .unwrap();
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
        let judged = configuration.judged();
        let tree = super::super::AnalysisTree::try_from_file(judged.file()).unwrap();
        let resource = tree.resources().next().unwrap();
        let slot = resource.authored_slot("input").unwrap();
        let proof = slot
            .projection()
            .unwrap()
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let mut coverage = WalkCoverage::default();
        let findings = crate::schema::value_validation::validate_authored_value_for_admission(
            &destination,
            &proof,
            &Default::default(),
            crate::schema::FieldAncestry::WRITABLE,
            &|_: &CompiledAttributeType, _: &Value| Ok(()),
            AuthoredCursor::authored(slot.cursor().unwrap()),
            RequiredAssignmentCallbacks {
                ordinary: &|_, _, _, _, _| Ok(Default::default()),
                authored: AuthoredChecks::Validation(&|attr, checked, path, cursor| {
                    check_conversion::<
                        crate::validation::admission::kind::ResourceAttribute,
                        ParserPlaceholder,
                        RelativeBindingName,
                    >(attr, checked, path, cursor, &Default::default(), &catalog)
                }),
                scope: AssignmentScope::Full,
            },
            &mut coverage,
        );
        assert_eq!(
            findings.local_errors().count(),
            1,
            "converted={converted}: {findings:?}"
        );
        assert_eq!(
            coverage.take_conversions().findings().len(),
            0,
            "the unavailable destination declines the slot before its children"
        );
    }
}

#[test]
fn r5b_resource_and_function_boundaries_cut_actual_producer_evidence() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut violations = Vec::new();
    for source in [
        "let p = mock.Source { output = 'ok' as mock.Name }\nlet consumer = p.output",
        "fn f(a: Int): Int { 1 as mock.Name }\nlet consumer = f(1)",
        "fn f(a: Int = 1 as mock.Name): Int { a }\nlet consumer = f()",
    ] {
        let parsed = super::super::parse_file(source, Path::new("producer.crn"), &catalog).unwrap();
        let report = parsed.expression_report();
        let producer = report
            .syntax_nodes
            .values()
            .find(|root| {
                matches!(
                    root.kind,
                    ExprKind::Resource { .. } | ExprKind::Function { .. }
                )
            })
            .expect("actual producer node");
        assert!(
            producer.children.iter().any(|child| report
                .node(&child.authored.location.instance, child.node_id())
                .is_some_and(JudgedExpr::contains_any_conversion)),
            "the cut must receive a carrying child: {source}"
        );
        if report
            .node(&producer.location.instance, producer.id)
            .unwrap()
            .contains_any_conversion()
        {
            violations.push(format!("producer boundary: {source}"));
        }
        if report
            .binding(&RelativeBindingName::new("consumer"))
            .unwrap()
            .contains_any_conversion()
        {
            violations.push(format!("plain consumer: {source}"));
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}

#[test]
fn r5b_typed_sink_rejects_a_carried_expression_without_a_placement_parent() {
    let catalog = super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = super::super::parse_file(
        "let x = 'ok' as unloaded.Name",
        Path::new("sink.crn"),
        &catalog,
    )
    .unwrap();
    let source = parsed
        .expression_report()
        .binding(&RelativeBindingName::new("x"))
        .unwrap();
    assert!(source.contains_any_conversion());
    assert!(
        admit_expression(source, &super::super::TypeExpr::String).is_err(),
        "a typed sink must reject a carrying expression even when P1 has no rejecting parent"
    );
    let rejection = match admit_expression(source, &super::super::TypeExpr::String) {
        Err(SinkRejection::Conversion(rejection)) => rejection,
        Err(SinkRejection::Ordinary(error)) => panic!("wrong finding channel: {error}"),
        Ok(admission) => panic!("unjudged conversion admitted: {admission:?}"),
    };
    let mut run = crate::validation::ConversionValidation::<std::convert::Infallible>::new(
        parsed.expression_report(),
    );
    run.reject_sink(rejection, crate::validation::FindingSubject::Root);
    let presented = run.complete().into_presented();
    assert_eq!(presented.len(), 1);
    assert!(matches!(
        presented.iter().next().unwrap(),
        crate::validation::PresentedFinding::Conversion(_)
    ));
}
