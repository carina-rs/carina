use super::*;

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
            ordinary: &|_, _, _, _| { ordinary.set(ordinary.get() + 1); Ok(Default::default()) },
            authored: AuthoredChecks::Validation(&|attr, checked, path, cursor| {
                if matches!(&cursor.kind, CursorKind::Authored(usage) | CursorKind::CoveredBy(usage) if usage.form() == AuthoredForm::Convert) { conversions.set(conversions.get() + 1); }
                check_conversion::<crate::validation::admission::kind::ResourceAttribute, ParserPlaceholder, RelativeBindingName>(attr, checked, path, cursor, schema.schema().as_schema().defs(), &catalog)
            }), scope: AssignmentScope::Full,
        }, &mut Default::default(),
    ).unwrap();
    assert_eq!(conversions.get(), 1);
    assert_eq!(
        ordinary.get(),
        2,
        "only the two parens wrappers retain ordinary checks; the claimed leaf has none"
    );
    assert_eq!(findings.authored.errors.len(), 1);
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
        let result = crate::schema::value_validation::validate_authored_value_for_admission(
            &schema.schema().as_schema().attributes()["input"].attr_type,
            &proof,
            schema.schema().as_schema().defs(),
            crate::schema::FieldAncestry::WRITABLE,
            &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
            AuthoredCursor::authored(slot.cursor().unwrap()),
            RequiredAssignmentCallbacks {
                ordinary: &|_, _, _, _| Ok(Default::default()),
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
                        };
                        let mut counts = verdicts.get();
                        counts[index] += 1;
                        verdicts.set(counts);
                    }
                    Ok(outcome)
                }),
                scope: AssignmentScope::Full,
            },
            &mut Default::default(),
        );
        assert!(result.local_error().is_none());
        assert_eq!(
            result.conversions().findings().len(),
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
    let result = crate::schema::value_validation::validate_authored_value_for_admission(
        &schema.schema().as_schema().attributes()["input"].attr_type,
        &proof,
        schema.schema().as_schema().defs(),
        crate::schema::FieldAncestry::WRITABLE,
        &|_: &crate::schema::CompiledAttributeType, _: &Value| Ok(()),
        AuthoredCursor::authored(slot.cursor().unwrap()),
        RequiredAssignmentCallbacks {
            ordinary: &|_, _, path, _| {
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
        &mut Default::default(),
    );
    assert!(result.local_error().is_none());
    assert_eq!(result.conversions().findings().len(), 1);
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
    let ExprKind::Convert { target } = &node.kind else {
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
        let error = super::super::parse_file(
            source,
            Path::new("convert.crn"),
            &super::super::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap_err();
        let conversion = crate::ConversionError::from_error(&error).unwrap_or_else(|| {
            panic!("parser rejection must retain the Convert: {source}: {error:?}")
        });
        assert_eq!(
            conversion.kind(),
            crate::ConversionErrorKind::UnsupportedPosition,
            "{error:?}"
        );
        assert!(conversion.location().span().start_column() > 1);
    }
}
