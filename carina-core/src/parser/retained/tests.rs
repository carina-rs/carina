use super::*;
use crate::parser::ProviderContext;
use crate::schema::{
    PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, SchemaRegistry,
};
use crate::static_type::{InferenceError, InferenceIssue};
use crate::validation::typecheck::{TypecheckInference, check_type_expr_value_providerless};

fn config() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
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
        TypecheckInference::Judged(expression),
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
        Err(InferenceIssue::Failed(InferenceError::UnknownType { .. }))
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

fn round2_report(source: &str) -> (crate::parser::ParsedFile, ExpressionReport) {
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
fn round2_if_admits_every_branch_at_the_sink() {
    for condition in ["true", "false"] {
        let (_, report) = round2_report(&format!(
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
fn round2_for_admits_the_body_as_the_list_element() {
    let (_, report) =
        round2_report("let result = for n in [1, 2] { mock.test.Source { name = \"${n}\" } }");
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(admit_expression(result, &TypeExpr::List(Box::new(TypeExpr::Int))).is_err());
}

#[test]
fn round2_coalesce_keeps_assignment_and_function_reference_policies() {
    let sink = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    for expression in ["\"${src.name}\" ?? src.name", "src.name ?? \"${src.name}\""] {
        let (_, report) = round2_report(&format!(
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
fn round2_literal_access_retains_string_construction_kind() {
    let sink = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "mock",
            crate::schema::TypeIdentity::new(Some("mock"), ["test"], "Name"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let (parsed, report) =
        round2_report("let src = mock.test.Source {}\nlet result = [\"${src.name}\"][0]");
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert_eq!(
        admit_expression(result, &sink),
        Ok(ExpressionAdmission::DeferredInterpolation)
    );
    check_type_expr_value_providerless(
        &sink,
        &parsed.variables["result"],
        &config(),
        TypecheckInference::Judged(result),
    )
    .require_provider_schemas()
    .unwrap();
}

#[test]
fn round2_module_parameter_uses_its_declaration_in_every_instance() {
    let (_, caller) =
        round2_report("let src = mock.test.Source {}\nlet supplied = \"${src.name}\"");
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
            [(
                RelativeBindingName::new("input"),
                TypeExpr::String,
                supplied.clone(),
            )],
        );
    let report = judge_expressions(&module, &SchemaRegistry::new(), &config());
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(
        crate::validation::typecheck::admit_function_expression(result, &sink, &config()).is_err()
    );
}

#[test]
fn round2_missing_child_evidence_is_a_hard_error() {
    let (_, report) = round2_report("let result = ['one']");
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
        TypecheckInference::Judged(result),
    )
    .require_provider_schemas();
    assert!(check.is_err(), "missing evidence was admitted: {check:?}");
}

#[test]
fn round2_analysis_reuses_evidence_and_registry() {
    let parsed = crate::parser::parse_and_resolve("let value = 'value'").unwrap();
    let schemas = SchemaRegistry::new();
    let config = config();
    let first = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config);
    let second = crate::config_loader::load_buffer_configuration(first.parsed(), &schemas, &config);
    assert!(
        std::ptr::eq(
            schemas.assignability_closures(),
            first.judged().schemas().assignability_closures()
        ),
        "registry was deep-cloned"
    );
    assert!(
        std::ptr::eq(
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
        "expression was judged again"
    );
}

#[test]
fn round2_composite_collections_preserve_child_construction_kinds() {
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
        let (parsed, report) = round2_report(&format!(
            "let src = mock.test.Source {{}}\nlet result = {source}"
        ));
        let expression = report.binding(&RelativeBindingName::new("result")).unwrap();
        let result = check_type_expr_value_providerless(
            &sink,
            &parsed.variables["result"],
            &config(),
            TypecheckInference::Judged(expression),
        )
        .require_provider_schemas();
        assert!(result.is_ok(), "{source}: {result:?}");
    }
}

#[test]
fn round2_wait_retains_the_target_handle_kind() {
    let (_, report) = round2_report(
        "let src = mock.test.Source {}\nlet ready = wait src { until = src.name == 'ready' }\nlet result = ready",
    );
    let handle = TypeExpr::Ref(crate::parser::ResourceTypePath::new("mock", "test.Source"));
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert_eq!(result.static_type(), Ok(&handle));
    assert!(admit_expression(result, &handle).is_ok());
}

#[test]
fn round2_directory_merge_judges_every_parameter_in_the_new_scope() {
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
fn round2_unloaded_function_annotation_keeps_the_scalar_floor() {
    let sink = TypeExpr::DottedUnresolved(crate::parser::ResourceTypePath::new(
        "mock",
        "test.Unavailable",
    ));
    for value in ["true", "['value']"] {
        let (_, report) = round2_report(&format!("let result = {value}"));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert!(
            crate::validation::typecheck::admit_function_expression(result, &sink, &config())
                .is_err(),
            "{value} was admitted without an annotation authority"
        );
    }
}

#[test]
fn round2_deferred_reads_have_resource_evidence() {
    use crate::parser::DeferredForPhaseTraversal;
    let (parsed, report) = round2_report(
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
fn round2_folded_function_calls_keep_argument_failure_evidence() {
    let (_, report) = round2_report(
        "fn choose(n: Int): String { 'selected' }\nlet src = mock.test.Source {}\nlet result = choose(src.name)",
    );
    let result = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert!(
        admit_expression(result, &TypeExpr::String).is_err(),
        "a folded call lost its failed argument check"
    );
}

#[test]
fn round2_declared_string_cannot_enter_an_unloaded_nominal_sink() {
    let (_, report) = round2_report("arguments { input: String }\nlet result = input");
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
fn round2_collection_failures_survive_a_shape_only_builtin() {
    for source in [
        "arguments { text: String }\nlet result = length([text, 42])",
        "arguments { text: String }\nlet result = length(text ?? [])",
        "arguments { text: String }\nlet result = length([] ?? text)",
    ] {
        let (_, report) = round2_report(source);
        assert!(
            !report.findings().is_empty(),
            "invalid collection operation was erased: {source}"
        );
    }
}

#[test]
fn round2_provider_authority_cannot_supply_a_strings_missing_nominal_identity() {
    let (_, report) = round2_report("arguments { input: String }\nlet result = input");
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
fn round2_builtin_collection_shape_survives_missing_element_inference() {
    for expression in ["concat(xs, xs)", "flatten([xs])", "values({ names = xs })"] {
        let (_, report) = round2_report(&format!(
            "arguments {{ xs: list(String) }}\nlet result = {expression}"
        ));
        let result = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert!(
            admit_expression(result, &TypeExpr::Int).is_err(),
            "{expression}"
        );
        assert_eq!(
            result.collection_kind(),
            Some(CollectionKind::List),
            "{expression}"
        );
        let (_, indexed) = round2_report(&format!(
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
fn round2_invalid_iterable_cannot_produce_admissible_elements() {
    let (_, report) = round2_report(
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
    let (_, report) = round2_report(
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
