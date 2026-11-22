use super::*;
use crate::parser::ProviderContext;
use crate::schema::{
    PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, SchemaRegistry,
};
use crate::static_type::{InferenceError, InferenceIssue};
use crate::validation::typecheck::check_type_expr_value_providerless;

fn config() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

#[test]
fn merged_syntax_invalidates_the_shared_secrecy_index() {
    let config = config();
    let schemas = SchemaRegistry::new();
    let first = crate::parser::parse_file(
        "let first = mock.Resource { name = 'first' }",
        Path::new("first.crn"),
        &config,
    )
    .unwrap();
    let second = crate::parser::parse_file(
        "let second = mock.Resource { name = 'second' }",
        Path::new("second.crn"),
        &config,
    )
    .unwrap();
    let first_location = first.parser_binding_metadata.syntax().expressions().roots()[0].location();
    let second_location = second
        .parser_binding_metadata
        .syntax()
        .expressions()
        .roots()[0]
        .location();
    let mut report = judge_expressions(&first, &schemas, &config);
    let second_report = judge_expressions(&second, &schemas, &config);
    assert!(report.secrecy_at(first_location).is_some());
    assert!(report.secrecy_at(second_location).is_none());
    report.extend(second_report);
    assert!(report.secrecy_at(first_location).is_some());
    assert!(report.secrecy_at(second_location).is_some());
}

#[test]
fn expressions_equality_includes_authored_secrecy_argument_roots() {
    let parsed =
        crate::parser::parse_file("let value = 'public'", Path::new("equality.crn"), &config())
            .unwrap();
    let location = parsed
        .parser_binding_metadata
        .syntax()
        .expressions()
        .roots()[0]
        .location()
        .clone();
    let original = Expressions::<RelativeBindingName>::default();
    let mut changed = original.clone();
    changed.secrecy_argument_roots.insert(
        ExpressionLocation::new(ExpressionInstance::Root, &location),
        "argument".into(),
    );
    assert_eq!(original, original.clone());
    assert_ne!(original, changed);
}

#[test]
fn heterogeneous_record_keeps_its_field_constraint_checks() {
    let parsed =
        crate::parser::parse_and_resolve("let value = { label = 'x' inner = { leaf = 'bad' } }")
            .unwrap();
    let config = config();
    let report = judge_expressions(&parsed, &SchemaRegistry::new(), &config);
    assert!(report.findings().is_empty(), "{:?}", report.findings());
    let expression = report.binding(&RelativeBindingName::new("value")).unwrap();
    let expected = TypeExpr::Struct {
        fields: vec![
            ("label".into(), TypeExpr::String),
            (
                "inner".into(),
                TypeExpr::Struct {
                    fields: vec![("leaf".into(), TypeExpr::Int)],
                },
            ),
        ],
    };
    assert_eq!(
        admit_expression(expression, &expected),
        Ok(ExpressionAdmission::ValueCheck)
    );
    let error = check_type_expr_value_providerless(
        &expected,
        &parsed.variables["value"],
        &config,
        (expression).ordinary().unwrap(),
    )
    .require_provider_schemas()
    .unwrap_err()
    .to_string();
    assert!(error.contains("leaf") && error.contains("Int"), "{error}");
}

#[test]
fn dynamic_call_needs_an_export_annotation_without_poisoning_known_results() {
    let parsed = crate::parser::parse_and_resolve(
        "let value = lookup({ a = 'one' }, 'a', 'default')\nlet count = length([lookup({ a = 'one' }, 'a', 'default')])",
    ).unwrap();
    let report = judge_expressions(&parsed, &SchemaRegistry::new(), &config());
    assert!(report.findings().is_empty(), "{:?}", report.findings());
    let value = report.binding(&RelativeBindingName::new("value")).unwrap();
    assert!(matches!(
        value.static_type(),
        Err(InferenceIssue::NotInferable(
            crate::static_type::NotInferableReason::ValueDependent { .. }
        ))
    ));
    assert_eq!(
        admit_expression(value, &TypeExpr::String),
        Ok(ExpressionAdmission::ValueCheck)
    );
    assert_eq!(
        report
            .binding(&RelativeBindingName::new("count"))
            .unwrap()
            .static_type(),
        Ok(&TypeExpr::Int)
    );
}

#[test]
fn each_coalesce_still_rejects_a_noninferable_fallback_before_folding() {
    let parsed = crate::parser::parse_and_resolve(
        "let a = 'present'\nlet first = a ?? { label = 'x' inner = { leaf = 1 } }\nlet second = a ?? { label = 'x' inner = { leaf = 1 } }",
    ).unwrap();
    let report = judge_expressions(&parsed, &SchemaRegistry::new(), &config());
    assert_eq!(report.findings().len(), 2, "{:?}", report.findings());
    for name in ["first", "second"] {
        let expression = report.binding(&RelativeBindingName::new(name)).unwrap();
        assert!(admit_expression(expression, &TypeExpr::String).is_err());
    }
    assert_ne!(
        report.findings()[0].location(),
        report.findings()[1].location()
    );
}

#[test]
fn known_builtin_results_retain_invalid_operand_accesses() {
    let parsed = crate::parser::parse_and_resolve(
        "let source = mock.test.Source {}\nlet result = length([source.name[0]])",
    )
    .unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let report = judge_expressions(&parsed, &schemas, &config());
    assert_eq!(report.findings().len(), 1, "{:?}", report.findings());
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(matches!(
        result.static_type(),
        Err(InferenceIssue::Failed(InferenceError::InvalidAccess { .. }))
    ));
    assert!(admit_expression(result, &TypeExpr::Int).is_err());
}

fn source_schema_report(source: &str) -> (crate::parser::ParsedFile, ExpressionReport) {
    let parsed = crate::parser::parse_and_resolve(source).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let report = judge_expressions(&parsed, &schemas, &config());
    (parsed, report)
}

#[test]
fn keyed_loop_groups_have_no_value_type_or_collection_sink() {
    for source in [
        "let items = { a = 'x' }\nlet children = for k, v in items { mock.test.Source { name = v } }",
        "let items = ['a']\nlet children = for item in items { mock.test.Source { name = item } }",
    ] {
        let (_, report) = source_schema_report(source);
        let group = report
            .binding(&RelativeBindingName::new("children"))
            .unwrap();
        assert!(
            group.static_type().is_err(),
            "{source}: {:?}",
            group.static_type()
        );
        for sink in [
            TypeExpr::Map(Box::new(TypeExpr::String)),
            TypeExpr::List(Box::new(TypeExpr::String)),
            TypeExpr::Struct {
                fields: vec![("a".into(), TypeExpr::String)],
            },
        ] {
            assert!(admit_expression(group, &sink).is_err(), "{source}: {sink}");
        }
    }
}

#[test]
fn keyed_loop_addressing_agrees_across_judgement_validation_and_expansion() {
    use crate::resource::{ForKeyError, ForLoopElement, LoopOriginKind};
    use crate::validation::DeferredForIterableTypeDiagnostic;

    fn loop_expression(expression: &ExprValue) -> Option<&ExprValue> {
        if matches!(expression.authored.kind, ExprKind::For { .. }) {
            return Some(expression);
        }
        expression
            .authored
            .children
            .iter()
            .find_map(loop_expression)
    }

    let config = config();
    let sink = TypeExpr::List(Box::new(TypeExpr::String));
    for (row, header, iterable, expected, expected_validation) in [
        (
            "bound indexed list",
            "let children = for (i, x) in ",
            "['a-b']",
            "Indexed",
            "Indexed",
        ),
        (
            "bound value-keyed list",
            "let children = for x in ",
            "['a-b']",
            "Keyed",
            "Keyed",
        ),
        (
            "unbound simple list",
            "for x in ",
            "['a-b']",
            "Indexed",
            "Indexed",
        ),
        (
            "bound map",
            "let children = for k, v in ",
            "{ 'a-b' = 'a-b' }",
            "Keyed",
            "MapKeyed",
        ),
        (
            "unbound map",
            "for k, v in ",
            "{ 'a-b' = 'a-b' }",
            "Keyed",
            "MapKeyed",
        ),
    ] {
        let body = "{ mock.test.Source { name = 'child' } }";
        let (parsed, report) =
            source_schema_report(&format!("let items = {iterable}\n{header}items {body}"));
        let expression = parsed
            .parser_binding_metadata
            .syntax()
            .expressions()
            .roots()
            .iter()
            .find_map(loop_expression)
            .unwrap();
        let judgement = match report
            .expression(expression.location())
            .unwrap()
            .for_sink(&sink, &config)
            .collection_kind()
        {
            Some(CollectionKind::KeyedGroup) => "Keyed",
            Some(CollectionKind::List) => "Indexed",
            other => panic!("{row}: unexpected judged group {other:?}"),
        };

        // An unresolved Int list reaches the authored validator without eager
        // parser admission. Only bound simple loops set value_keys and reject
        // its element type. Map loops never enter that path: their container
        // rejection is a separate MapKeyed observation.
        let probe = crate::parser::parse_file(
            &format!("arguments {{ items: list(Int) }}\n{header}items {body}"),
            Path::new("agreement.crn"),
            &config,
        )
        .unwrap();
        let schemas = SchemaRegistry::new();
        let buffer = crate::config_loader::load_buffer_configuration(&probe, &schemas, &config);
        let tree = crate::parser::AnalysisTree::try_from(buffer.parsed()).unwrap();
        let findings = crate::validation::validate_for_iterable_types(&tree, &config);
        let findings: Vec<_> = findings.iter().map(|finding| finding.detail()).collect();
        let validation = match findings.as_slice() {
            [] => "Indexed",
            [
                DeferredForIterableTypeDiagnostic::ForKey(ForKeyError::ElementType { kind: "Int" }),
            ] => "Keyed",
            [DeferredForIterableTypeDiagnostic::Iterable { message, .. }]
                if message == "expected map iterable, got list" =>
            {
                "MapKeyed"
            }
            other => panic!("{row}: unexpected validation findings {other:?}"),
        };

        // Eager bound expansion calls admit_bound_for_addresses; unbound
        // expansion records the actual key/index in its child provenance.
        let resources: Vec<_> = parsed.iter_all_resources().collect();
        let [crate::parser::ResourceRef::Resource(child)] = resources.as_slice() else {
            panic!("{row}: expected one expanded child");
        };
        let expansion = match child.binding.as_ref().map(|binding| binding.as_str()) {
            Some("children['a-b']") => "Keyed",
            Some("children[0]") => "Indexed",
            None => match child.loop_provenance.direct_origin().unwrap().kind() {
                LoopOriginKind::For {
                    element: ForLoopElement::Key(key),
                } if key == "a-b" => "Keyed",
                LoopOriginKind::For {
                    element: ForLoopElement::Index(0),
                } => "Indexed",
                other => panic!("{row}: unexpected expanded child origin {other:?}"),
            },
            other => panic!("{row}: unexpected expanded child address {other:?}"),
        };
        assert_eq!(
            [judgement, validation, expansion],
            [expected, expected_validation, expected],
            "{row}: [judge, validation, expansion]"
        );
    }
}

#[test]
fn loop_dot_judgement_selects_a_key_and_owns_indexed_rejection() {
    for (pattern, iterable) in [("k, v", "({ a = 'x' })"), ("v", "['a']")] {
        let (_, report) = source_schema_report(&format!(
            "let children = for {pattern} in {iterable} {{ mock.test.Source {{ name = v }} }}\nlet selected = children.a.name"
        ));
        let selected = report
            .binding(&RelativeBindingName::new("selected"))
            .unwrap();
        assert_eq!(selected.static_type().unwrap(), &TypeExpr::String);
        assert!(report.findings().is_empty(), "{:?}", report.findings());
    }
    let (_, report) = source_schema_report(
        "let children = for (i, v) in ['a'] { mock.test.Source { name = v } }\nlet selected = children.a.name",
    );
    let errors: Vec<_> = report
        .findings()
        .iter()
        .map(|finding| finding.detail().to_string())
        .collect();
    assert_eq!(errors, ["cannot apply access .a to list(mock.test.Source)"]);
}

#[test]
fn single_reference_interpolation_preserves_child_judgement_and_literal_folding() {
    for definition in [
        "let x = src.name",
        "arguments { x: String }",
        "let x = 'valid'",
        "let x = 'INVALID'",
        "let x = src.missing",
        "let x = 'present' ?? 42",
    ] {
        for interpolation in ["\"${x}\"", "\"${ x }\""] {
            let (parsed, report) = source_schema_report(&format!(
                "let src = mock.test.Source {{}}\n{definition}\nlet direct = x\nlet wrapped = {interpolation}"
            ));
            let direct = report.binding(&RelativeBindingName::new("direct")).unwrap();
            let wrapped = report
                .binding(&RelativeBindingName::new("wrapped"))
                .unwrap();
            assert_eq!(wrapped.static_type(), direct.static_type(), "{definition}");
            assert_eq!(wrapped.check_owner(), direct.check_owner(), "{definition}");
            assert_eq!(
                wrapped
                    .for_sink(&TypeExpr::String, &config())
                    .requires_declared_assignment(),
                direct
                    .for_sink(&TypeExpr::String, &config())
                    .requires_declared_assignment(),
                "{definition}"
            );
            assert_eq!(
                wrapped
                    .for_sink(&TypeExpr::String, &config())
                    .is_single_string_construction(),
                direct
                    .for_sink(&TypeExpr::String, &config())
                    .is_single_string_construction(),
                "{definition}"
            );
            for sink in [TypeExpr::String, TypeExpr::Int, TypeExpr::Bool] {
                assert_eq!(
                    admit_expression(wrapped, &sink),
                    admit_expression(direct, &sink),
                    "{definition}: {sink}"
                );
            }
            if matches!(
                (parsed.variables["direct"])
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification()),
                Ok(crate::resource::knowledge::Classification::Known(_))
            ) {
                assert_eq!(parsed.variables["wrapped"], parsed.variables["direct"]);
            }
        }
    }
}

#[test]
fn single_reference_interpolation_stringifies_non_string_scalars() {
    for (definition, expected) in [
        ("let x = 42", Some("42")),
        ("let x = 1.5", Some("1.5")),
        ("let x = true", Some("true")),
        ("let x = 1min", None),
        ("arguments { x: Int }", None),
        ("arguments { x: Float }", None),
        ("arguments { x: Bool }", None),
    ] {
        let (parsed, report) =
            source_schema_report(&format!("{definition}\nlet wrapped = \"${{x}}\""));
        let wrapped = report
            .binding(&RelativeBindingName::new("wrapped"))
            .unwrap();
        assert_eq!(wrapped.static_type(), Ok(&TypeExpr::String), "{definition}");
        assert!(
            wrapped
                .for_sink(&TypeExpr::String, &config())
                .is_single_string_construction(),
            "{definition}"
        );
        assert!(admit_expression(wrapped, &TypeExpr::String).is_ok());
        assert!(admit_expression(wrapped, &TypeExpr::Int).is_err());
        if let Some(expected) = expected {
            assert_eq!(
                parsed.variables["wrapped"],
                Value::Concrete(crate::resource::ConcreteValue::String(expected.into())),
                "{definition}"
            );
        }
    }
}

#[test]
fn if_admits_every_branch_at_the_sink() {
    for condition in ["true", "false"] {
        let (_, report) = source_schema_report(&format!(
            "let src = mock.test.Source {{}}\nlet result = if {condition} {{ 'selected' }} else {{ \"${{src.name}}\" }}"
        ));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        for sink in [
            TypeExpr::Int,
            TypeExpr::Bool,
            TypeExpr::List(Box::new(TypeExpr::String)),
        ] {
            assert!(
                admit_expression(result, &sink).is_err(),
                "{condition}: {sink}"
            );
        }
    }
}

#[test]
fn for_admits_the_body_as_the_list_element() {
    let (_, report) = source_schema_report(
        "let result = for (_, n) in [1, 2] { mock.test.Source { name = \"${n}\" } }",
    );
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(admit_expression(result, &TypeExpr::List(Box::new(TypeExpr::Int))).is_err());
}

#[test]
fn coalesce_keeps_assignment_and_function_reference_policies() {
    let sink = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    for expression in ["\"${src.name}\" ?? src.name", "src.name ?? \"${src.name}\""] {
        let (_, report) = source_schema_report(&format!(
            "let src = mock.test.Source {{}}\nlet result = {expression}"
        ));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert!(admit_expression(result, &sink).is_err(), "{expression}");
        assert!(
            crate::validation::typecheck::admit_function_expression(result, &sink, &config())
                .is_ok(),
            "function references retain apply-time validation: {expression}"
        );
    }
}

#[test]
fn literal_access_preserves_interpolation_judgement() {
    let sink = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    for (interpolation, accepted) in [("\"${src.name}\"", false), ("\"${src.name}-inline\"", true)]
    {
        let (parsed, report) = source_schema_report(&format!(
            "let src = mock.test.Source {{}}\nlet result = [{interpolation}][0]"
        ));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert_eq!(admit_expression(result, &sink).is_ok(), accepted);
        assert_eq!(
            check_type_expr_value_providerless(
                &sink,
                &parsed.variables["result"],
                &config(),
                (result).ordinary().unwrap()
            )
            .require_provider_schemas()
            .is_ok(),
            accepted
        );
    }
}

#[test]
fn module_parameter_uses_its_declaration_in_every_instance() {
    let (_, caller) =
        source_schema_report("let src = mock.test.Source {}\nlet supplied = \"${src.name}\"");
    let supplied = caller
        .binding(&RelativeBindingName::new("supplied"))
        .unwrap();
    let sink = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    assert!(admit_expression(supplied, &TypeExpr::String).is_ok());
    let mut module =
        crate::parser::parse_and_resolve("arguments { input: String }\nlet result = input")
            .unwrap();
    // Binding qualification is deliberately performed by the normal expansion API.
    let scope = crate::resource::ModuleInstanceScope::for_call("template", Some("child"), "child");
    module
        .parser_binding_metadata
        .syntax
        .expressions_mut()
        .bind_arguments(
            &scope,
            [(RelativeBindingName::new("input"), TypeExpr::String)],
        );
    let report = judge_expressions(&module, &SchemaRegistry::new(), &config());
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(
        crate::validation::typecheck::admit_function_expression(result, &sink, &config()).is_err()
    );
}

#[test]
fn missing_child_evidence_is_a_hard_error() {
    let (_, report) = source_schema_report("let result = ['one']");
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    let projection: Value<ParserPlaceholder, RelativeBindingName> =
        Value::Concrete(crate::resource::ConcreteValue::List(vec![
            Value::Concrete(crate::resource::ConcreteValue::String("one".into())),
            Value::Concrete(crate::resource::ConcreteValue::String("two".into())),
        ]));
    let check = check_type_expr_value_providerless(
        &TypeExpr::List(Box::new(TypeExpr::String)),
        &projection,
        &config(),
        (result).ordinary().unwrap(),
    )
    .require_provider_schemas();
    assert!(check.is_err(), "missing evidence was admitted: {check:?}");
}

#[test]
fn analysis_borrows_registry_and_judges_a_fresh_report() {
    let parsed = crate::parser::parse_and_resolve("let value = 'value'").unwrap();
    let schemas = SchemaRegistry::new();
    let config = config();
    let first = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config);
    let second = crate::config_loader::load_buffer_configuration(first.parsed(), &schemas, &config);
    assert!(
        std::ptr::eq(&schemas, first.judged().schemas()),
        "registry was deep-cloned"
    );
    assert!(
        !std::ptr::eq(
            first
                .judged()
                .report()
                .binding(&RelativeBindingName::new("value"))
                .unwrap(),
            second
                .judged()
                .report()
                .binding(&RelativeBindingName::new("value"))
                .unwrap()
        ),
        "new analysis reused an earlier report"
    );
}

#[test]
fn composite_collections_preserve_child_interpolation_judgements() {
    let named = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    for (source, sink) in [
        (
            "[\"${src.name}\"] ?? [\"${src.name}\"]",
            TypeExpr::List(Box::new(named.clone())),
        ),
        (
            "if false { [] } else { [\"${src.name}\", \"${src.name}\"] }",
            TypeExpr::List(Box::new(named.clone())),
        ),
        (
            "{ value = \"${src.name}\" } ?? { value = \"${src.name}\" }",
            TypeExpr::Map(Box::new(named)),
        ),
    ] {
        for (source, accepted) in [
            (source.to_string(), false),
            (source.replace("${src.name}", "${src.name}-inline"), true),
        ] {
            let (parsed, report) = source_schema_report(&format!(
                "let src = mock.test.Source {{}}\nlet result = {source}"
            ));
            let expression = report.binding(&RelativeBindingName::new("result")).unwrap();
            let result = check_type_expr_value_providerless(
                &sink,
                &parsed.variables["result"],
                &config(),
                (expression).ordinary().unwrap(),
            )
            .require_provider_schemas();
            assert_eq!(result.is_ok(), accepted, "{source}: {result:?}");
        }
    }
}

#[test]
fn wait_retains_the_target_handle_kind() {
    let (_, report) = source_schema_report(
        "let src = mock.test.Source {}\nlet ready = wait src { until = src.name == 'ready' }\nlet result = ready",
    );
    let handle = TypeExpr::Ref(crate::parser::ResourceTypePath::new("mock", "test.Source"));
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert_eq!(result.static_type(), Ok(&handle));
    assert!(admit_expression(result, &handle).is_ok());
}

#[test]
fn directory_merge_judges_every_parameter_in_the_new_scope() {
    let context = config();
    let schemas = SchemaRegistry::new();
    let mut first =
        crate::parser::parse_file("arguments { first: Int }", Path::new("first.crn"), &context)
            .unwrap();
    let mut second = crate::parser::parse_file(
        "arguments { second: String }",
        Path::new("second.crn"),
        &context,
    )
    .unwrap();
    judge_configuration(&mut first, &schemas, &context);
    judge_configuration(&mut second, &schemas, &context);
    first.merge_from(second);
    judge_configuration(&mut first, &schemas, &context);
    assert_eq!(
        first
            .expression_report()
            .parameter("second")
            .unwrap()
            .static_type(),
        Ok(&TypeExpr::String)
    );
}

#[test]
fn unloaded_function_annotation_keeps_the_scalar_floor() {
    let sink = TypeExpr::DottedUnresolved(crate::parser::ResourceTypePath::new(
        "mock",
        "test.Unavailable",
    ));
    for value in ["true", "['value']"] {
        let (_, report) = source_schema_report(&format!("let result = {value}"));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert!(
            crate::validation::typecheck::admit_function_expression(result, &sink, &config())
                .is_err(),
            "{value} was admitted without an annotation authority"
        );
    }
}

#[test]
fn deferred_reads_have_resource_evidence() {
    use crate::parser::DeferredForPhaseTraversal;
    let (parsed, report) = source_schema_report(
        "let src = mock.test.Source {}\nlet reads = for n in src.name { read mock.test.Source {} }",
    );
    let reads: Vec<_> = parsed.deferred_for_phase.analysis_resource_refs().collect();
    assert_eq!(reads.len(), 1);
    for read in reads {
        assert!(
            report.for_resource(read.id()).is_ok(),
            "missing read evidence for {}",
            read.id()
        );
    }
}

#[test]
fn folded_function_calls_keep_argument_failure_evidence() {
    let (_, report) = source_schema_report(
        "fn choose(n: Int): String { 'selected' }\nlet src = mock.test.Source {}\nlet result = choose(src.name)",
    );
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(
        admit_expression(result, &TypeExpr::String).is_err(),
        "a folded call lost its failed argument check"
    );
}

#[test]
fn declared_string_cannot_enter_an_unloaded_nominal_sink() {
    let (_, report) = source_schema_report("arguments { input: String }\nlet result = input");
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    let sink = TypeExpr::DottedUnresolved(crate::parser::ResourceTypePath::new(
        "mock",
        "test.Unavailable",
    ));
    assert!(
        crate::validation::typecheck::admit_function_expression(result, &sink, &config()).is_err()
    );
}

#[test]
fn collection_failures_survive_a_shape_only_builtin() {
    for source in [
        "arguments { text: String }\nlet result = length([text, 42])",
        "arguments { text: String }\nlet result = length(text ?? [])",
        "arguments { text: String }\nlet result = length([] ?? text)",
    ] {
        let (_, report) = source_schema_report(source);
        assert!(
            !report.findings().is_empty(),
            "invalid collection operation was erased: {source}"
        );
    }
}

#[test]
fn provider_authority_cannot_supply_a_strings_missing_nominal_identity() {
    let (_, report) = source_schema_report("arguments { input: String }\nlet result = input");
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    let sink = crate::parser::resolve_custom_type_identity(
        &crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
        &config(),
    )
    .unwrap()
    .unwrap();
    assert!(matches!(sink, TypeExpr::UnloadedSchemaType(_)));
    assert!(admit_expression(result, &sink).is_err());
    assert!(
        crate::validation::typecheck::admit_function_expression(result, &sink, &config()).is_err()
    );
}

#[test]
fn builtin_collection_shape_survives_missing_element_inference() {
    for expression in ["concat(xs, xs)", "flatten([xs])", "values({ names = xs })"] {
        let (_, report) = source_schema_report(&format!(
            "arguments {{ xs: list(String) }}\nlet result = {expression}"
        ));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert!(
            admit_expression(result, &TypeExpr::Int).is_err(),
            "{expression}"
        );
        assert_eq!(
            result
                .for_sink(&TypeExpr::List(Box::new(TypeExpr::String)), &config())
                .collection_kind(),
            Some(CollectionKind::List),
            "{expression}"
        );
        let (_, indexed) = source_schema_report(&format!(
            "arguments {{ xs: list(String) }}\nlet result = {expression}[0]"
        ));
        let element = indexed
            .binding(&RelativeBindingName::new("result"))
            .unwrap();
        assert!(
            admit_expression(element, &TypeExpr::String).is_ok(),
            "{expression}"
        );
    }
}

#[test]
fn invalid_iterable_cannot_produce_admissible_elements() {
    let (_, report) = source_schema_report(
        "let src = mock.test.Source {}\nlet result = for n in src.name { mock.test.Source {} }",
    );
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    let expected = TypeExpr::List(Box::new(TypeExpr::Ref(
        crate::parser::ResourceTypePath::new("mock", "test.Source"),
    )));
    assert!(admit_expression(result, &expected).is_err());
}

#[test]
fn failure_ownership_survives_wrappers_and_delegates_reference_errors() {
    let (_, report) = source_schema_report(
        "let src = mock.test.Source {}\nlet bad = src.missing\nlet intrinsic = length(['present' ?? 42])",
    );
    assert_eq!(report.findings().len(), 1, "{:?}", report.findings());
    let intrinsic = report
        .binding(&RelativeBindingName::new("intrinsic"))
        .unwrap();
    assert_eq!(intrinsic.check_owner(), ExpressionCheckOwner::Reported);
    assert!(admit_expression(intrinsic, &TypeExpr::Int).is_err());
    let reference = report.binding(&RelativeBindingName::new("bad")).unwrap();
    assert_eq!(reference.check_owner(), ExpressionCheckOwner::BindingIndex);
    assert!(admit_expression(reference, &TypeExpr::Int).is_err());
}

#[test]
fn resource_sink_rejects_a_declared_list_without_the_binding_pass() {
    let (_, report) = source_schema_report(
        "arguments { input: list(String) }\nlet target = mock.test.Source { name = input }",
    );
    let target = report.binding(&RelativeBindingName::new("target")).unwrap();
    let handle = TypeExpr::Ref(crate::parser::ResourceTypePath::new("mock", "test.Source"));
    assert!(admit_expression(target, &handle).is_err());
}

#[test]
fn annotation_sink_rejects_a_declared_list_without_the_binding_pass() {
    let mut parsed = crate::parser::parse_and_resolve(
        "arguments { input: list(String) }\nattributes { output: String = input }",
    )
    .unwrap();
    judge_configuration(&mut parsed, &SchemaRegistry::new(), &config());
    let expression = parsed.attribute_params[0].judgement().unwrap();
    assert!(
        admit_expression(expression, &TypeExpr::List(Box::new(TypeExpr::String))).is_err(),
        "the rejected String sink must remain rejected even against its source type"
    );
}

#[test]
fn declared_containers_fail_closed_at_scalar_sinks() {
    for ty in [
        "list(String)",
        "map(String)",
        "struct { label: String }",
        "list(String) | map(String)",
        "Ipv4Address",
    ] {
        let (_, report) =
            source_schema_report(&format!("arguments {{ input: {ty} }}\nlet result = input"));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert_eq!(result.check_owner(), ExpressionCheckOwner::BindingIndex);
        assert!(admit_expression(result, &TypeExpr::Int).is_err(), "{ty}");
        if ty != "Ipv4Address" {
            assert!(admit_expression(result, &TypeExpr::String).is_err(), "{ty}");
        }
    }
}

#[test]
fn module_parameter_judgement_is_independent_of_the_callers_value() {
    let (_, caller) = source_schema_report(
        "let src = mock.test.Source {}\nlet literal = 'value'\nlet deferred = src.name",
    );
    let mut admissions = Vec::new();
    for name in ["literal", "deferred"] {
        assert!(
            admit_expression(
                caller.binding(&RelativeBindingName::new(name)).unwrap(),
                &TypeExpr::String
            )
            .is_ok()
        );
        let mut module = crate::parser::parse_and_resolve(
            "arguments { input: String }\nlet result = \"${input}\"",
        )
        .unwrap();
        let scope = crate::resource::ModuleInstanceScope::for_call("template", Some(name), name);
        module
            .parser_binding_metadata
            .syntax
            .expressions_mut()
            .bind_arguments(
                &scope,
                [(RelativeBindingName::new("input"), TypeExpr::String)],
            );
        let report = judge_expressions(&module, &SchemaRegistry::new(), &config());
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        admissions.push(admit_expression(result, &TypeExpr::String));
    }
    assert_eq!(admissions[0], admissions[1]);
    assert_eq!(admissions[0], Ok(ExpressionAdmission::ValueCheck));
}

#[test]
fn resolved_nominal_declarations_fail_closed_at_unrelated_scalars() {
    let named = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let mut module = crate::parser::parse_and_resolve(
        "arguments { input: String }\nlet direct = input\nlet result = \"${input}\"",
    )
    .unwrap();
    let scope = crate::resource::ModuleInstanceScope::for_call("template", Some("child"), "child");
    module
        .parser_binding_metadata
        .syntax
        .expressions_mut()
        .bind_arguments(&scope, [(RelativeBindingName::new("input"), named.clone())]);
    let report = judge_expressions(&module, &SchemaRegistry::new(), &config());
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    let direct = report.binding(&RelativeBindingName::new("direct")).unwrap();
    assert_eq!(result.static_type(), Ok(&named));
    assert_eq!(result.check_owner(), direct.check_owner());
    assert_eq!(
        result
            .for_sink(&TypeExpr::String, &config())
            .requires_declared_assignment(),
        direct
            .for_sink(&TypeExpr::String, &config())
            .requires_declared_assignment()
    );
    assert_eq!(
        result
            .for_sink(&TypeExpr::String, &config())
            .is_single_string_construction(),
        direct
            .for_sink(&TypeExpr::String, &config())
            .is_single_string_construction()
    );
    assert!(admit_expression(result, &TypeExpr::Int).is_err());
    assert!(admit_expression(result, &TypeExpr::Bool).is_err());
}

#[test]
fn boundary_value_check_enforces_binding_owned_admission() {
    let (parsed, report) =
        source_schema_report("arguments { input: list(String) }\nlet result = input");
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert_eq!(result.check_owner(), ExpressionCheckOwner::BindingIndex);
    let context = config();
    let location = parsed
        .parser_binding_metadata
        .syntax()
        .expressions()
        .binding(&RelativeBindingName::new("result"))
        .unwrap()
        .location()
        .clone();
    let checked = crate::validation::typecheck::TypecheckProviderContext::providerless(&context)
        .check_boundary_values_at(
            &TypeExpr::String,
            &parsed.variables["result"],
            result.ordinary().unwrap(),
            crate::validation::deferred_types::ProviderTypeCheckSite::new(
                location,
                "attribute 'output'",
            ),
        );
    assert!(
        checked.is_err(),
        "the value checker admitted a binding-owned list into String"
    );
}

#[test]
fn interpolated_conditional_waits_for_its_condition() {
    let parsed = crate::parser::parse_and_resolve(
        "arguments { enabled: Bool }\nlet result = \"${if enabled { 'valid' } else { 'also' }}\"",
    )
    .unwrap();
    let report = judge_expressions(&parsed, &SchemaRegistry::new(), &config());
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert_eq!(
        admit_expression(result, &TypeExpr::String),
        Ok(ExpressionAdmission::DeferredInterpolation)
    );
}

#[test]
fn issue_4282_deferred_builtin_construction_matches_interpolation_admission() {
    let sink = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    for expression in [
        "join('', [src.name, '-suffix'])",
        "upper(src.name)",
        "lower(src.name)",
        "replace('a', 'b', src.name)",
        "trim(src.name)",
        "cidr_subnet(src.name, 4, 1)",
        "secret(join('', [src.name, '-suffix']))",
        "split(',', src.name)[0]",
    ] {
        let (_, report) = source_schema_report(&format!(
            "let src = mock.test.Source {{}}\nlet result = {expression}"
        ));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert_eq!(result.static_type(), Ok(&TypeExpr::String), "{expression}");
        assert!(
            result
                .for_sink(&sink, &config())
                .is_single_string_construction(),
            "{expression}"
        );
        assert_eq!(
            admit_expression(result, &sink),
            Ok(ExpressionAdmission::DeferredInterpolation),
            "{expression}",
        );
        assert!(
            admit_expression(result, &TypeExpr::Int).is_err(),
            "{expression}"
        );
    }
    let (_, report) =
        source_schema_report("let src = mock.test.Source {}\nlet result = split(',', src.name)");
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert_eq!(
        admit_expression(result, &TypeExpr::List(Box::new(sink))),
        Ok(ExpressionAdmission::DeferredInterpolation),
    );
    assert!(admit_expression(result, &TypeExpr::List(Box::new(TypeExpr::Int))).is_err());
}

#[test]
fn issue_4282_round2_collection_members_never_become_a_single_construction() {
    let config = config();
    let strings = TypeExpr::List(Box::new(TypeExpr::String));
    let ints = TypeExpr::List(Box::new(TypeExpr::Int));
    for expression in [
        r#"["${''}A", "${''}B"]"#,
        r#"["${src.name}-a", "${''}B"]"#,
        "[join('', [src.name, '-a']), upper('b')]",
        "split(',', src.name)",
    ] {
        let (_, report) = source_schema_report(&format!(
            "let src = mock.test.Source {{}}\nlet result = {expression}"
        ));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        let judged = result.for_sink(&strings, &config);
        assert!(!judged.is_single_string_construction(), "{expression}");
        assert!(judged.has_constructed_members(), "{expression}");
        assert_eq!(judged.collection_kind(), Some(CollectionKind::List));
        assert!(
            result
                .element(0)
                .unwrap()
                .for_sink(&TypeExpr::String, &config)
                .is_single_string_construction()
        );
        if expression.starts_with('[') && expression.contains("src.name") {
            assert_eq!(
                admit_expression(result.element(0).unwrap(), &TypeExpr::String),
                Ok(ExpressionAdmission::DeferredInterpolation)
            );
            assert_eq!(
                admit_expression(result.element(1).unwrap(), &TypeExpr::String),
                Ok(ExpressionAdmission::ValueCheck)
            );
        }
        if expression.contains("upper(") || expression.starts_with("split(") {
            assert!(!result.for_sink(&ints, &config).has_constructed_members());
            // Reading the same evidence at another sink cannot erase it.
            assert!(result.for_sink(&strings, &config).has_constructed_members());
        }
    }
}

#[test]
fn last_owned_deep_retained_syntax_drops_iteratively() {
    let parsed = crate::parser::parse("let leaf = 'value'", &config()).unwrap();
    let mut root = parsed
        .parser_binding_metadata
        .syntax()
        .expressions()
        .roots()[0]
        .clone();
    let location = root.location().clone();
    for _ in 0..20_000 {
        root = node_at(location.clone(), ExprKind::Alias, vec![root]);
    }
    let shared = root.clone();
    drop(root);
    drop(shared);
}

#[test]
fn condition_history_lookup_and_drop_use_heap_storage() {
    let first = ConditionRecorder::default();
    let known = NodeId::fresh();
    first.extend([(known, true)]);
    let mut snapshot = first.snapshot();
    let missing = NodeId::fresh();
    for _ in 0..20_000 {
        let next = ConditionRecorder::default();
        next.extend([(NodeId::fresh(), false)]);
        snapshot = next.snapshot().over(&snapshot);
    }
    assert_eq!(snapshot.get(known), Some(true));
    assert_eq!(snapshot.get(missing), None);
    drop(snapshot);
}

#[test]
fn structural_secrecy_is_lazy_and_owned_by_the_immutable_judged_graph() {
    let config = config();
    let schemas = SchemaRegistry::new();
    let parsed = crate::parser::parse("let input = secret('value') ?? 'public'", &config).unwrap();
    let judged = JudgedFile::new(parsed, &schemas, &config);
    assert!(judged.secrecy.get().is_none());
    assert!(judged.report().findings().is_empty());
    assert!(
        judged.secrecy.get().is_none(),
        "type queries must not derive secrecy"
    );

    let secrecy = judged.secrecy();
    assert_eq!(secrecy.findings().len(), 1);
    assert_eq!(
        secrecy.findings()[0].cause(),
        &crate::secrecy::DerivationError::MixedAlternatives(crate::secrecy::MixedAlternatives {
            operator: crate::secrecy::Alternatives::Coalesce,
        })
    );
    assert!(std::ptr::eq(secrecy, judged.secrecy()));
    let findings = secrecy.findings().to_vec();

    let rejudged = JudgedFile::new(judged.into_file(), &schemas, &config);
    assert!(
        rejudged.secrecy.get().is_none(),
        "a new graph must derive its own evidence"
    );
    assert_eq!(rejudged.secrecy().findings(), findings.as_slice());
}
