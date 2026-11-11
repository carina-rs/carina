static TEST_OWNER: std::sync::LazyLock<crate::parser::SchemaTypeOwner> =
    std::sync::LazyLock::new(|| crate::parser::SchemaTypeOwner::Provider("test".into()));

use super::*;
use crate::parser::{
    ArgumentOrigin, LoadedProviderContext, ModuleArgument, ParsedFile, ProviderContext,
    RelativeBindingName, UntilPredicateAst, WaitBinding,
};
use crate::resource::{
    AbsoluteBindingAddress, AuthoredCompositionCall as CompositionCall,
    AuthoredCompositionCallPath as CompositionCallPath, AuthoredDataSource as DataSource,
    AuthoredLiveComposition as LiveComposition,
    AuthoredLiveCompositionArgument as LiveCompositionArgument,
    AuthoredLiveCompositionAttribute as LiveCompositionAttribute,
    AuthoredLiveCompositionSignature as LiveCompositionSignature, AuthoredResource as Resource,
    NoPlaceholder, RootScope,
};
use crate::resource::{
    AuthoredConcreteValue as ConcreteValue, AuthoredDeferredValue as DeferredValue,
};
use crate::schema::{
    PatternAttributeSchema, PatternAttributeType, PatternResourceSchema, PatternStructField,
};
use crate::schema::{SchemaRegistry, TypeIdentity};
use std::collections::{BTreeMap, HashMap, HashSet};

type RuntimeValue = GenericValue<NoPlaceholder, AbsoluteBindingAddress>;
type RuntimeConcreteValue = crate::resource::ConcreteValue<NoPlaceholder, AbsoluteBindingAddress>;
type RuntimeDeferredValue = crate::resource::DeferredValue<NoPlaceholder, AbsoluteBindingAddress>;
type RuntimeInterpolationPart =
    crate::resource::InterpolationPart<NoPlaceholder, AbsoluteBindingAddress>;
type RuntimeInferredExportParam =
    crate::parser::InferredExportParam<NoPlaceholder, AbsoluteBindingAddress>;
type RelativeRuntimeValue = GenericValue<NoPlaceholder, RelativeBindingName>;

#[test]
fn export_inference_preserves_authored_failure() {
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    for rhs in [
        "'present' ?? 42",
        "length(['present' ?? 42])",
        "\"${'present' ?? 42}\"",
    ] {
        let parsed =
            crate::parser::parse_and_resolve(&format!("exports {{ result = {rhs} }}")).unwrap();
        let inferred = inference::apply_inference(parsed, &SchemaRegistry::new(), &config);
        assert!(
            matches!(
                &inferred.export_params[0].static_type,
                crate::static_type::ExportStaticType::Inferred(
                    crate::static_type::StaticType::Unchecked(
                        crate::static_type::InferredUncheckedReason::InferenceFailed(
                            inference::InferenceError::IncompatibleCoalesceTypes { .. }
                        )
                    )
                )
            ),
            "{rhs}: {:?}",
            inferred.export_params[0].static_type
        );
    }
}

#[test]
fn module_instances_retain_substituted_judgements() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    std::fs::write(
        root.path().join("child/main.crn"),
        "arguments { x: String }\nattributes { result = \"${x}\" }\n",
    )
    .unwrap();
    std::fs::write(root.path().join("main.crn"), "let source = mock.test.Source {}\nlet template = use { source = './child' }\nlet literal = template { x = 'present' }\nlet deferred = template { x = source.name }\n").unwrap();
    let path = crate::paths::ResolvedPath::new(root.path()).unwrap();
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let directory = crate::config_loader::parse_directory(&path, &config).unwrap();
    let graph = deferred_types::with_session_for_test(&config, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            directory,
            &path,
            session,
            &SchemaRegistry::new(),
        )
    })
    .unwrap();
    let report = graph.expression_report();
    let scope = RootScope::new();
    let literal = report
        .module_attribute(
            &scope.resolve(RelativeBindingName::new("literal")),
            "result",
        )
        .unwrap();
    let deferred = report
        .module_attribute(
            &scope.resolve(RelativeBindingName::new("deferred")),
            "result",
        )
        .unwrap();
    assert_eq!(
        crate::parser::retained::admit_expression(literal, &TypeExpr::String),
        Ok(crate::parser::retained::ExpressionAdmission::ValueCheck)
    );
    assert_eq!(
        crate::parser::retained::admit_expression(deferred, &TypeExpr::String),
        Ok(crate::parser::retained::ExpressionAdmission::ValueCheck)
    );
}

#[test]
fn single_reference_integer_interpolation_checks_resolved_string_constraints() {
    let identity = TypeIdentity::new(Some("mock"), ["test"], "Digits");
    let schema = PatternResourceSchema::new("test.Source")
        .attribute(PatternAttributeSchema::optional("port", PatternAttributeType::int()).build())
        .attribute(
            PatternAttributeSchema::optional(
                "digits",
                PatternAttributeType::refined_string(
                    Some(identity.clone()),
                    Some(crate::schema::PatternSpec::new(
                        "^[0-9]+$",
                        crate::schema::PatternMatch::Full,
                    )),
                    Some((Some(2), Some(4))),
                    None,
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let expected = TypeExpr::SchemaType {
        owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
            "mock",
            schema.as_schema().attributes["digits"].attr_type(),
        )
        .unwrap(),
    };
    let config = loaded_context_for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                "mock",
                identity,
                crate::schema::RefinedScalarKind::String,
            )]),
            Box::new(|_, _| Ok(())),
        ),
        ..expression_validation_config()
    });
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema);
    let parsed = crate::parser::parse_and_resolve(
        "let src = mock.test.Source {}\nlet port = src.port\nlet result = \"${port}\"",
    )
    .unwrap();
    let report = crate::parser::retained::judge_expressions(&parsed, &schemas, &config);
    let expression = report.binding(&RelativeBindingName::new("result")).unwrap();
    assert_eq!(
        crate::parser::retained::admit_expression(expression, &expected),
        Ok(crate::parser::retained::ExpressionAdmission::DeferredInterpolation)
    );
    let authored = parsed.variables["result"]
        .clone()
        .resolve_root(&RootScope::new())
        .into_expanded()
        .unwrap();
    for (port, accepted) in [(1, false), (42, true), (12345, false), (-42, false)] {
        let source = Resource::with_provider("mock", "test.Source", "src", None)
            .with_binding("src")
            .with_attribute("port", Value::Concrete(ConcreteValue::Int(port)))
            .resolve_root(&RootScope::new())
            .into_expanded(&schemas)
            .unwrap();
        let bindings = crate::binding_index::ResolvedBindings::pre_apply_live(
            crate::binding_index::LivePreApplyInputs {
                managed: &[source],
                compositions: &[],
                managed_current: &HashMap::new(),
                data_source_values: &crate::data_source_value::DataSourceValues::empty(),
                remote_bindings: &HashMap::new(),
                wait_aliases: &[],
            },
        );
        let resolved = deferred_types::with_session_for_test(&config, |session| {
            crate::resolver::resolve_ref_value_with_config(&authored, &bindings, session)
        })
        .unwrap();
        assert_eq!(
            resolved,
            RuntimeValue::Concrete(RuntimeConcreteValue::String(port.to_string().into()))
        );
        let checked = typecheck::check_type_expr_value(&expected, &resolved, &config, expression)
            .require_provider_schemas();
        assert_eq!(checked.is_ok(), accepted, "{port}: {checked:?}");
    }
    assert!(
        !matches!(
            (authored)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification()),
            Ok(crate::resource::knowledge::Classification::Known(_))
        ),
        "checking must preserve the deferred source"
    );
}

#[test]
fn validation_views_borrow_the_produced_report() {
    let parsed = crate::parser::parse_and_resolve("let unused = 'present' ?? 42").unwrap();
    let schemas = SchemaRegistry::new();
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
    let judged = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config);
    let first = ValidationTypeContext::new_providerless(judged.judged(), &config, &signatures);
    let second = ValidationTypeContext::new_providerless(judged.judged(), &config, &signatures);
    assert!(std::ptr::eq(
        first.expression_report,
        second.expression_report
    ));
    assert!(std::ptr::eq(
        first.expression_report,
        judged.judged().report()
    ));
    assert_eq!(first.expression_report.findings().len(), 1);
}

#[test]
fn planning_resolution_requires_a_successful_judgement() {
    let schemas = SchemaRegistry::new();
    let provider = LoadedProviderContext::empty_for_test();
    let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
    let unjudged = crate::parser::ModuleResolvedParsedFile::default();
    assert!(matches!(
        resolve_bare_binding_uses(unjudged, &schemas, &provider, &signatures),
        Err(ResolveFileError::Unjudged(_))
    ));
    let parsed = crate::parser::parse_and_resolve("let unused = 'present' ?? 42")
        .unwrap()
        .resolve_root(&RootScope::new());
    let (invalid, ()) = parsed.map_deferred_for_phase(|templates| {
        assert!(templates.is_empty());
        (Vec::new(), ())
    });
    assert!(matches!(
        resolve_bare_binding_uses(invalid, &schemas, &provider, &signatures),
        Err(ResolveFileError::Expression(_))
    ));
}

#[test]
fn deferred_directive_consumes_judged_iterable_type() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("main.crn"), "let source = mock.r.Typed { name = 'source' tags = { child = 'not-a-bool' } }\nlet children = for child, flag in source.tags { mock.r.Typed { name = child directives { prevent_destroy = flag } } }\n").unwrap();
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("r.Typed")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "tags",
                    PatternAttributeType::map(PatternAttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let path = crate::paths::ResolvedPath::new(root.path()).unwrap();
    let loaded =
        crate::config_loader::load_configuration_with_config(&path, &config, &schemas).unwrap();
    let graph = deferred_types::with_session_for_test(&config, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            loaded.parsed,
            &path,
            session,
            &schemas,
        )
    })
    .unwrap();
    let site = graph.deferred_for_sites().next().unwrap();
    let expression = graph
        .expression_report()
        .expression(site.iterable_location.unwrap());
    assert_eq!(
        expression
            .as_ref()
            .ok()
            .map(|expression| expression.static_type()),
        Some(Ok(&TypeExpr::Map(Box::new(TypeExpr::String)))),
        "{expression:?}"
    );
    let errors = graph
        .directive_resolution_errors(
            crate::parser::ParserPlaceholderBoundary::ExpandedFile,
            &schemas,
            &config,
        )
        .unwrap();
    assert!(
        !errors.is_empty(),
        "loop String was admitted to a Bool directive"
    );
}

#[test]
fn authored_expression_judges_coalesce_before_enclosing_reduction() {
    let mut missed = Vec::new();
    for expression in [
        "'present' ?? 42",
        "length(['present' ?? 42])",
        "['ok', ('present' ?? 42)][0]",
        "\"${'present' ?? 42}\"",
        "length({key = 'present' ?? 42})",
        "secret('present' ?? 42)",
        "['present' ?? 42]",
        "{key = 'present' ?? 42}",
    ] {
        let parsed =
            crate::parser::parse_and_resolve(&format!("let result = {expression}\n")).unwrap();
        let report = crate::parser::retained::judge_expressions(
            &parsed,
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        let result = report
            .binding(&RelativeBindingName::new("result"))
            .unwrap()
            .static_type();
        let expected_failure = if expression.starts_with("secret(") {
            for rendered in [
                result.as_ref().unwrap_err().to_string(),
                format!("{result:?}"),
            ] {
                assert!(!rendered.contains("present"), "{rendered}");
                assert!(!rendered.contains("42"), "{rendered}");
            }
            matches!(
                result,
                Err(crate::static_type::InferenceIssue::Failed(
                    inference::InferenceError::IncompatibleCoalesceTypes { .. }
                ))
            )
        } else {
            matches!(
                result,
                Err(crate::static_type::InferenceIssue::Failed(
                    inference::InferenceError::IncompatibleCoalesceTypes { .. }
                ))
            )
        };
        if !expected_failure {
            missed.push(format!("{expression}: {result:?}"));
        }
    }
    assert!(
        missed.is_empty(),
        "unchecked authored operands were erased:\n{}",
        missed.join("\n")
    );
}

#[test]
fn authored_expression_module_argument_substitution_cannot_erase_fallback() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join("child")).unwrap();
    std::fs::write(
        root.path().join("child/main.crn"),
        "arguments { x: String }\nlet unused = length([x ?? 42])\nattributes { result = 'ok' }\n",
    )
    .unwrap();
    std::fs::write(
        root.path().join("main.crn"),
        "let template = use { source = './child' }\nlet child = template { x = 'present' }\n",
    )
    .unwrap();
    std::fs::write(root.path().join("exports.crn"), "exports {}\n").unwrap();
    std::fs::write(root.path().join("providers.crn"), "").unwrap();
    std::fs::write(
        root.path().join("backend.crn"),
        "backend local { path = 'state.json' }\n",
    )
    .unwrap();
    let path = crate::paths::ResolvedPath::new(root.path()).unwrap();
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let loaded = crate::config_loader::load_configuration_with_config(
        &path,
        &config,
        &SchemaRegistry::new(),
    )
    .unwrap();
    let result = deferred_types::with_session_for_test(&config, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            loaded.parsed,
            &path,
            session,
            &SchemaRegistry::new(),
        )
    });
    assert!(
        result.is_err(),
        "module argument substitution erased an invalid unused expression"
    );
}

#[test]
fn authored_expression_nested_operands_and_lexical_scopes_survive() {
    let mut missed = Vec::new();
    for source in [
        "let result = ['ok'][0 ?? 'bad']",
        "let result = 'present' ?? 'fallback' ?? 42",
        "let result = length({ nested { value = 'present' ?? 42 } })",
        "test.r.typed { let a = 'present' plain = length([a ?? 42]) }",
        "for a in ['present'] { test.r.typed { plain = a ?? 42 } }",
        "fn bad(a: String): String { let b = a b ?? 42 }",
    ] {
        let parsed = crate::parser::parse_and_resolve(source).unwrap();
        let report = crate::parser::retained::judge_expressions(
            &parsed,
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        if report.findings().is_empty() {
            missed.push(source);
        }
    }
    assert!(
        missed.is_empty(),
        "unchecked lexical/index operands: {missed:#?}"
    );
}

#[test]
fn authored_expression_judgement_does_not_evaluate_lazy_or_secret_values() {
    const SECRET: &str = "expression-typing-secret-must-not-leak";
    for source in [
        "let result = 'present' ?? env('CARINA_UNSELECTED_I4255_MISSING')".to_string(),
        format!("let result = 'present' ?? secret('{SECRET}')"),
    ] {
        let parsed = crate::parser::parse_and_resolve(&source).unwrap();
        let report = crate::parser::retained::judge_expressions(
            &parsed,
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert!(report.findings().is_empty(), "{:?}", report.findings());
        assert_eq!(
            parsed.variables["result"],
            Value::Concrete(ConcreteValue::String("present".into()))
        );
        let judged = report.binding(&RelativeBindingName::new("result")).unwrap();
        assert_eq!(judged.static_type().unwrap(), &TypeExpr::String);
        assert!(!format!("{judged:?}").contains(SECRET));
    }
}

#[test]
fn authored_expression_reference_authority_is_lexical() {
    for source in [
        "let a = { field = [1] }\nlet result = a.field ?? 'bad'",
        "arguments { cfg: struct { items: list(String) } }\nlet result = length(cfg.items ?? 42)",
    ] {
        let parsed = crate::parser::parse_and_resolve(source).unwrap();
        let report = crate::parser::retained::judge_expressions(
            &parsed,
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            report.findings().len(),
            1,
            "{source}: {:?}",
            report.findings()
        );
    }
}

#[test]
fn authored_expression_structural_consumers_keep_unselected_operands() {
    for source in [
        "provider mock { version = '1.0.0' ?? 42 }",
        "provider mock { default_tags = { key = 'value' } ?? 42 }",
        "backend local { path = 'state.json' ?? 42 }",
        "if (true ?? 42) { mock.test.resource { name = 'selected' } }",
        "for name in (['selected'] ?? 42) { mock.test.resource { name = name } }",
    ] {
        let parsed = crate::parser::parse_and_resolve(source).unwrap();
        let report = crate::parser::retained::judge_expressions(
            &parsed,
            &SchemaRegistry::new(),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(
            report.findings().len(),
            1,
            "{source}: {:?}",
            report.findings()
        );
    }
}

#[test]
fn authored_expression_union_sink_cannot_admit_incompatible_operands() {
    let parsed = crate::parser::parse_and_resolve("let value = 'present' ?? 42").unwrap();
    let report = crate::parser::retained::judge_expressions(
        &parsed,
        &SchemaRegistry::new(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let expression = report.binding(&RelativeBindingName::new("value")).unwrap();
    assert!(
        crate::parser::retained::admit_expression(
            expression,
            &TypeExpr::Union(vec![TypeExpr::String, TypeExpr::Int])
        )
        .is_err()
    );
}

fn local_type(type_expr: TypeExpr) -> TypeExpr {
    type_expr
}

fn consumer_type(type_expr: TypeExpr) -> crate::static_type::ConsumerStaticType {
    resolve_consumer_static_type_for_signature(
        &type_expr,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
}

fn module_arguments(
    values: impl IntoIterator<Item = (String, Value)>,
) -> HashMap<String, ModuleArgument> {
    values
        .into_iter()
        .map(|(name, value)| (name, ModuleArgument::new(value)))
        .collect()
}

fn rendered_typecheck_result(
    type_expr: &TypeExpr,
    value: &Value,
    config: &ProviderContext,
) -> Option<String> {
    typecheck::check_type_expr_value_with_context(
        type_expr,
        value,
        typecheck::TypecheckProviderContext::for_parser(config),
        &crate::parser::retained::value_dependent_judgement_for_test(),
    )
    .require_provider_schemas()
    .err()
    .map(|error| error.to_string())
}

#[test]
fn type_resolution_rejects_missing_provenance_without_panicking() {
    let mut parsed =
        crate::parser::parse_and_resolve("let source = mock.test.Resource {}\n").unwrap();
    parsed.resources[0].declaration = Default::default();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        deferred_types::with_session_for_test(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| resolve_file_type_exprs_report(&mut parsed, session, None),
        )
    }));
    let error = outcome
        .expect("must not panic")
        .expect_err("reject missing provenance");
    assert!(matches!(
        error.kind(),
        crate::parser::AnalysisTreeErrorKind::MissingResourceProvenance { .. }
    ));
}

#[test]
fn ref_validation_rejects_missing_provenance_without_panicking() {
    let mut parsed = crate::parser::parse_and_resolve("let source = mock.test.Resource {}\nlet up = upstream_state { source = './up' }\nlet copies = for item in up.items { mock.test.Resource {} }\n").unwrap();
    parsed.resources[0].declaration = Default::default();
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        deferred_types::with_session_for_test(
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            |session| {
                let schemas = SchemaRegistry::new();
                let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
                let bindings = BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(&parsed, &schemas, None, &signatures, session);
                let configuration = crate::config_loader::load_buffer_configuration(
                    &parsed,
                    &schemas,
                    session.as_provider_context(),
                );
                validate_resource_ref_types(
                    configuration.judged(),
                    &HashSet::new(),
                    &bindings,
                    &mut Vec::new(),
                )
            },
        )
    }));
    let error = outcome
        .expect("must not panic")
        .expect_err("reject missing provenance");
    assert!(matches!(
        error,
        ResourceRefTypeValidationError::AnalysisInvariant(_)
    ));
}

#[test]
fn deferred_iterable_shape_classifies_concrete_list_without_placeholder_type() {
    let shape = deferred_iterable_shape(
        &Value::Concrete(ConcreteValue::List(Vec::new())),
        &inference::InferenceBindings::new(),
        &SchemaRegistry::new(),
    )
    .expect("concrete list shape");

    assert_eq!(shape, DeferredIterableShape::List);
}

#[test]
fn deferred_iterable_shape_classifies_concrete_map_without_placeholder_type() {
    let shape = deferred_iterable_shape(
        &Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        &inference::InferenceBindings::new(),
        &SchemaRegistry::new(),
    )
    .expect("concrete map shape");

    assert_eq!(shape, DeferredIterableShape::Map);
}

#[test]
fn deferred_iterable_shape_preserves_non_iterable_scalar_type() {
    let shape = deferred_iterable_shape(
        &Value::Concrete(ConcreteValue::Int(1)),
        &inference::InferenceBindings::new(),
        &SchemaRegistry::new(),
    )
    .expect("concrete scalar shape");

    assert_eq!(shape, DeferredIterableShape::NonIterable(TypeExpr::Int));
}

#[test]
fn consumer_resolution_is_distinct_from_parser_types_and_preserves_inference_failure() {
    use crate::parser::{
        ArgumentParameter, InferredExportParam, ResourceTypePath, ValidationBlock,
    };
    use crate::static_type::{
        ConsumerExportStaticType, ConsumerInferredUncheckedReason, ExportStaticType,
        InferenceError, InferredUncheckedReason, StaticType,
    };

    let unknown = TypeExpr::DottedUnresolved(ResourceTypePath::new("missing", "Thing"));
    let mut parsed = ParsedFile::default();
    parsed.arguments.push(ArgumentParameter {
        name: "argument".to_string(),
        static_type: unknown.clone(),
        default: None,
        description: None,
        validations: Vec::<ValidationBlock<ParserPlaceholder, RelativeBindingName>>::new(),
    });
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter::dummy(
            "attribute".to_string(),
            Some(unknown.clone()),
            None,
        ));
    parsed
        .export_params
        .push(crate::parser::ParsedExportParam::dummy(
            "parsed_export".to_string(),
            Some(unknown.clone()),
            None,
        ));

    let argument = resolve_consumer_static_type(
        &parsed.arguments[0].static_type,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(matches!(
        &argument,
        StaticType::Unchecked(reason)
            if reason.original() == &unknown
                && matches!(
                    reason.resolution_error(),
                    crate::static_type::StaticTypeResolutionError::UnknownCustomType { .. }
                )
    ));
    let attributes = resolve_attribute_static_types_for_consumer(
        &parsed.attribute_params,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(matches!(
        &attributes[0].static_type,
        Some(StaticType::Unchecked(reason)) if reason.original() == &unknown
    ));
    let parsed_export = resolve_consumer_static_type(
        parsed.export_params[0]
            .static_type
            .as_ref()
            .expect("declared export type"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(matches!(
        &parsed_export,
        StaticType::Unchecked(reason) if reason.original() == &unknown
    ));

    let failure = InferenceError::UnknownType {
        reason: "dynamic".to_string(),
    };
    let inferred = ParsedFile::default().map_export_params(|_exports| {
        vec![
            InferredExportParam::<ParserPlaceholder, RelativeBindingName> {
                source: Default::default(),
                name: "inferred_export".to_string(),
                static_type: ExportStaticType::Inferred(StaticType::Known(unknown.clone())),
                value: None,
            },
            InferredExportParam::<ParserPlaceholder, RelativeBindingName> {
                source: Default::default(),
                name: "failed_export".to_string(),
                static_type: ExportStaticType::Inferred(StaticType::Unchecked(
                    InferredUncheckedReason::InferenceFailed(failure.clone()),
                )),
                value: None,
            },
        ]
    });

    let mut resolved = inferred.export_params.into_iter().map(|export| {
        resolve_export_static_type_for_consumer(
            export.static_type,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
    });

    assert!(matches!(
        resolved.next().expect("resolved inferred export"),
        ConsumerExportStaticType::Inferred(StaticType::Unchecked(
            ConsumerInferredUncheckedReason::UnnameableInConsumer(reason)
        )) if reason.original() == &unknown
    ));
    assert_eq!(
        resolved.next().expect("resolved failed export"),
        ConsumerExportStaticType::Inferred(StaticType::Unchecked(
            ConsumerInferredUncheckedReason::InferenceFailed(failure)
        ))
    );
}

fn rendered_struct_typecheck_result(
    fields: &[(String, TypeExpr)],
    entries: &IndexMap<String, Value>,
) -> Option<String> {
    rendered_typecheck_result(
        &TypeExpr::Struct {
            fields: fields.to_vec(),
        },
        &Value::Concrete(ConcreteValue::Map(entries.clone())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
}

fn empty_parsed() -> ParsedFile {
    ParsedFile {
        providers: Vec::new(),
        resources: Vec::new(),
        data_sources: Vec::new(),
        compositions: Vec::new(),
        variables: IndexMap::new(),
        uses: Vec::new(),
        module_calls: Vec::new(),
        arguments: Vec::new(),
        attribute_params: Vec::new(),
        export_params: vec![],
        backend: None,
        state_blocks: Vec::new(),
        user_functions: HashMap::new(),
        upstream_states: Vec::new(),
        wait_bindings: Vec::new(),
        requires: Vec::new(),
        structural_bindings: HashSet::new(),
        parser_binding_metadata: Default::default(),
        parsed_binding_uses: Default::default(),
        warnings: Vec::new(),
        deferred_for_phase: crate::parser::ParsedDeferredForPhase::default(),
        template_placeholder: std::marker::PhantomData,
        deferred_module_ifs: Vec::new(),
        expansion_trace: crate::resource::ExpansionTrace::new(),
        authored_inputs: None,
    }
}

#[test]
fn anonymous_schema_error_groups_plain_and_deferred_template_resources() {
    let parsed = crate::parser::parse(
        r#"
mock.test.Thing { name = "one" }
mock.test.Thing { name = "two" }

let upstream = upstream_state { source = "../upstream" }
for item in upstream.items {
    mock.test.Thing { name = item.name }
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture parses");
    assert_eq!(parsed.iter_top_level_resources().count(), 2);
    assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);

    let error = validate_resources(
        &parsed,
        &SchemaRegistry::new(),
        &HashSet::new(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    )
    .expect_err("an unloaded provider cannot derive anonymous identities");

    assert_eq!(
        error
            .lines()
            .filter(|line| line.contains("anonymous mock.test.Thing"))
            .collect::<Vec<_>>(),
        vec![
            "3 anonymous mock.test.Thing resources need provider 'mock', which is not declared or could not be loaded; declare provider 'mock' so its schema is available"
        ],
    );
}

fn module_call_findings_for_test(
    parsed: &ParsedFile,
    signatures: &crate::module_resolver::ResolvedModuleSignatures,
    provider_context: &LoadedProviderContext,
) -> Vec<ModuleCallFinding> {
    let schemas = SchemaRegistry::new();
    let mut fixture = parsed.clone();
    crate::parser::retained::judge_configuration(
        &mut fixture,
        &schemas,
        provider_context.as_provider_context(),
    );
    let parsed = &fixture;
    let binding_use_report = check_bare_binding_uses(&ValidationTypeContext::new(
        crate::config_loader::load_buffer_configuration(
            parsed,
            &schemas,
            provider_context.as_provider_context(),
        )
        .judged(),
        provider_context,
        signatures,
    ));
    let bindings = BindingIndex::from_parsed_with_module_signatures(
        parsed,
        &schemas,
        signatures,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    crate::validation::deferred_types::with_session_for_test(provider_context, |session| {
        check_module_calls(ModuleCallCheckInput {
            authored: parsed,
            semantic: Some(parsed),
            base_dir: Some(&crate::paths::ResolvedPath::new(std::path::Path::new(".")).unwrap()),
            imported_signatures: signatures,
            provider_context: session,
            schemas: &schemas,
            binding_use_report: &binding_use_report,
            bindings: &bindings,
            reference_checks: ModuleCallReferenceChecks::Enabled,
        })
        .unwrap()
    })
}

fn authored_binding_view(source: &str) -> crate::config_loader::AuthoredBindingView {
    crate::config_loader::AuthoredBindingView::parse_source(
        source,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse")
}

#[test]
fn ref_walker_does_not_inherit_function_result_sink_for_arguments() {
    let value = Value::Deferred(DeferredValue::FunctionCall {
        name: crate::builtins::BuiltinFunction::upper,
        args: vec![Value::resource_ref("source".to_string(), "value".to_string(), vec![]).into()],
        source_line: (0).into(),
    });
    let result_type = TypeExpr::String;
    let mut visited = Vec::new();

    visit_refs_with_sink_inner(
        &value,
        Some(RefSink::TypeExpr(&result_type)),
        crate::resource::ResourceRefUsage::Required,
        DiagnosticScope::Public,
        &mut |path, sink, _usage, _candidate, _scope| {
            visited.push((path.to_dot_string(), sink.is_none()))
        },
    );

    assert_eq!(
        visited,
        vec![("source.value".to_string(), true)],
        "the walker must visit function arguments for existence checks without applying the result sink",
    );
}

fn validate_resource_ref_types_for_test<
    E: crate::parser::ExportParamLike<ParserPlaceholder, RelativeBindingName> + Clone,
>(
    parsed: &crate::parser::File<E>,
    schemas: &SchemaRegistry,
) -> Result<(), String> {
    let mut fixture = parsed.clone();
    crate::parser::retained::judge_configuration(
        &mut fixture,
        schemas,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let parsed = &fixture;
    crate::validation::deferred_types::with_session_for_test(
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        |session| {
            let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
            let bindings = crate::binding_index::BindingIndex::from_parsed_with_upstream_exports_and_module_signatures_in_session(parsed, schemas, None, &signatures, session);
            let argument_names = parsed
                .arguments
                .iter()
                .map(|argument| argument.name.clone())
                .collect();
            let configuration = crate::config_loader::load_buffer_configuration(
                parsed,
                schemas,
                session.as_provider_context(),
            );
            super::validate_resource_ref_types(
                configuration.judged(),
                &argument_names,
                &bindings,
                &mut Vec::new(),
            )
            .map_err(|error| error.to_string())
        },
    )
}

#[test]
fn unused_let_integer_arithmetic_requires_proven_int_operands() {
    let parsed = crate::parser::parse(
        r#"
let source = mock.test.Source {}
let invalid = source.ratio + 1
let sink = mock.test.Consumer { count = invalid }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse before schema-aware validation");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::provider_populated("ratio", PatternAttributeType::float())
                    .build(),
            )
            .validate()
            .expect("source schema must be valid"),
    );
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Consumer")
            .attribute(
                PatternAttributeSchema::required("count", PatternAttributeType::int()).build(),
            )
            .validate()
            .expect("consumer schema must be valid"),
    );

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("an unused arithmetic let must still prove both operands are Int");
    assert_eq!(
        error, "test.crn:3:28: operator '+': left operand must be Int, got Float",
        "the declaration and its consuming resource attribute must not duplicate the finding",
    );
}

#[test]
fn loaded_catalog_rejects_undeclared_bare_integer_operand_type() {
    use crate::integer_arithmetic::{IntegerOp, IntegerOperatorSpan, evaluate_integer_values};

    let unknown = crate::parser::parse_type_expr_str(
        "UndeclaredCount",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let parsed = ParsedFile::default();
    let schemas = SchemaRegistry::new();
    let config = loaded_context_for_test(ProviderContext::new(crate::EnvironmentSnapshot::empty()));
    let index = BindingIndex::from_parsed(&parsed, &schemas, &config);
    let authored = crate::parser::parse_and_resolve(
        "arguments { count: UndeclaredCount }\nlet computed = count + 1",
    )
    .unwrap();
    let report = crate::parser::retained::judge_expressions(
        &authored,
        &schemas,
        config.as_provider_context(),
    );
    let expression = report
        .binding(&RelativeBindingName::new("computed"))
        .unwrap();
    let _ = unknown;
    let value = evaluate_integer_values(
        Value::Deferred(DeferredValue::Placeholder(
            ParserPlaceholder::ModuleArgument {
                name: "count".into(),
                diagnostic: Default::default(),
            },
        )),
        IntegerOp::Add,
        Value::Concrete(ConcreteValue::Int(1)),
        IntegerOperatorSpan::synthetic(),
    )
    .unwrap();
    let sink = AttributeType::int();
    let error = check_deferred_access_type(
        &value,
        crate::schema::TypeInSchema::schemaless(&sink).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER),
        ),
        expression,
        &schemas,
        &index,
    )
    .expect_err("a loaded catalog must reject an unknown operand type");
    assert!(
        matches!(&error, DeferredAccessTypeError::InvalidAccess {
        error: inference::InferenceError::UnknownType { reason },
    } if reason.contains("UndeclaredCount")),
        "{error}"
    );
}

#[test]
fn composition_reference_reports_declared_type_resolution_failure() {
    let parsed = crate::parser::parse(
        "let source = mock.test.Source {}",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::provider_populated("value", PatternAttributeType::string())
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let config = loaded_context_for_test(ProviderContext::new(crate::EnvironmentSnapshot::empty()));
    let index = BindingIndex::from_parsed(&parsed, &schemas, &config);
    let declared = crate::parser::parse_type_expr_str(
        "UndeclaredAccountId",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let path = crate::resource::AuthoredAccessPath::new("source", "value");
    let result = composition_ref_error_kind(
        (&path).into(),
        Some(RefSink::TypeExpr(&declared)),
        &index,
        UnknownReferencePolicy::Report,
        crate::resource::ResourceRefUsage::Required,
    );
    assert!(
        matches!(result, Some(ModuleCallRefErrorKind::DeclarationResolution(
        StaticTypeResolutionError::UnknownCustomType { name },
    )) if name == "UndeclaredAccountId"),
        "a direct reference check must retain the declaration's own error"
    );
}

#[test]
fn refined_int_resource_operand_is_still_an_int_operand() {
    let parsed = crate::parser::parse(
        r#"
let source = mock.test.Source {}
let valid = source.count + 1
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse before schema-aware validation");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "count",
                    PatternAttributeType::refined_int(Some(TypeIdentity::bare("Count")), None),
                )
                .build(),
            )
            .validate()
            .expect("source schema must be valid"),
    );

    validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect("a refined Int retains its Int arithmetic shape");
}

#[test]
fn refined_string_resource_operand_is_not_an_int_operand() {
    let parsed = crate::parser::parse(
        r#"
let source = mock.test.Source {}
let invalid = source.count + 1
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse before schema-aware validation");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::provider_populated(
                    "count",
                    PatternAttributeType::refined_string(
                        Some(TypeIdentity::bare("CountLabel")),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .expect("source schema must be valid"),
    );

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("a refined String must not become an arithmetic Int operand");
    assert!(error.contains("left operand must be Int"), "{error}");
}

#[test]
fn deferred_string_resource_attribute_is_not_an_int_operand() {
    let parsed = crate::parser::parse(
        "let source = mock.test.Source {}\nlet invalid = source.label + 1\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse before schema-aware validation");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::provider_populated("label", PatternAttributeType::string())
                    .build(),
            )
            .validate()
            .expect("source schema must be valid"),
    );

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("a deferred String attribute cannot be an arithmetic operand");
    assert_eq!(
        error,
        "test.crn:2:28: operator '+': left operand must be Int, got String"
    );
}

#[test]
fn integer_arithmetic_result_obeys_the_declared_resource_sink() {
    let parsed = crate::parser::parse(
        r#"
let source = mock.test.Source {}
let invalid = mock.test.Consumer { label = source.count + 1 }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("sink fixture must parse before schema-aware validation");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::provider_populated("count", PatternAttributeType::int())
                    .build(),
            )
            .validate()
            .expect("source schema must be valid"),
    );
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Consumer")
            .attribute(
                PatternAttributeSchema::required("label", PatternAttributeType::string()).build(),
            )
            .validate()
            .expect("consumer schema must be valid"),
    );

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("an Int expression cannot satisfy a String resource sink");
    assert!(
        error.contains("expected String") && error.contains("got Int"),
        "unexpected result-sink diagnostic: {error}"
    );
}

#[test]
fn module_attribute_integer_arithmetic_keeps_unliftable_sink_unchecked() {
    let parsed = crate::parser::parse(
        "arguments { a: Int = 1 }\nattributes { p: Port = a + 1 }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("an unloaded custom-type annotation must parse");

    validate_resource_ref_types_for_test(&parsed, &SchemaRegistry::new())
        .expect("a sink without a known declaration must remain unchecked");
}

#[test]
fn module_attribute_integer_arithmetic_uses_declared_argument_types() {
    let valid = crate::parser::parse(
        r#"
arguments { count: Int }
attributes { doubled: Int = count * 2 }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("valid module fixture must parse");
    validate_resource_ref_types_for_test(&valid, &SchemaRegistry::new())
        .expect("a declared Int module argument proves the arithmetic operand");

    let invalid = crate::parser::parse(
        r#"
arguments { ratio: Float }
attributes { rounded: Int = ratio + 1 }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("invalid module fixture must parse before type validation");
    let error = validate_resource_ref_types_for_test(&invalid, &SchemaRegistry::new())
        .expect_err("a declared Float module argument cannot be used in integer arithmetic");
    assert_eq!(
        error,
        "test.crn:3:35: operator '+': left operand must be Int, got Float"
    );
}

#[test]
fn unused_function_integer_arithmetic_uses_declared_parameter_types() {
    let valid = crate::parser::parse(
        "fn doubled(value: Int): Int { value * 2 }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("valid function fixture must parse");
    validate_resource_ref_types_for_test(&valid, &SchemaRegistry::new())
        .expect("a declared Int function parameter proves the arithmetic operand");

    let invalid = crate::parser::parse(
        "fn rounded(value: Float): Int { value + 1 }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("invalid function fixture must parse before type validation");
    let error = validate_resource_ref_types_for_test(&invalid, &SchemaRegistry::new())
        .expect_err("an unused function must still reject a Float arithmetic operand");
    assert_eq!(
        error,
        "test.crn:1:39: operator '+': left operand must be Int, got Float"
    );

    let invalid_return = crate::parser::parse(
        "fn mislabeled(value: Int): String { value + 1 }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("return-sink fixture must parse before directory type validation");
    let error = validate_resource_ref_types_for_test(&invalid_return, &SchemaRegistry::new())
        .expect_err("an arithmetic Int result cannot satisfy a declared String return type");
    assert!(
        error.contains("expected String") && error.contains("got Int"),
        "unexpected return-sink diagnostic: {error}"
    );

    let untyped = crate::parser::parse(
        "fn unproven(value): Int { value + 1 }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("untyped-parameter fixture must parse before directory type validation");
    let error = validate_resource_ref_types_for_test(&untyped, &SchemaRegistry::new())
        .expect_err("an untyped function parameter cannot prove an Int operand");
    assert!(
        error.contains("left operand must be Int") && error.contains("unproven type"),
        "unexpected untyped-operand diagnostic: {error}"
    );
}

fn function_ref_schema() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (PatternResourceSchema::new("test.resource").attribute(
            PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

#[test]
fn function_call_reference_arguments_use_the_declared_parameter_type() {
    let parsed = crate::parser::parse(
        r#"
let r = mock.test.resource { name = "r" }
fn f(x: Int) { "fixed" }
mock.test.resource { name = f(r.name) }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let schemas = function_ref_schema();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let error = validate_user_function_ref_types_with_bindings(&parsed, &bindings)
        .expect_err("String resource attribute passed to Int must be rejected");
    assert!(
        error.contains("expected Int") && error.contains("got String"),
        "{error}"
    );
}

#[test]
fn function_call_reference_in_parser_side_deferred_body_is_checked() {
    let parsed = crate::parser::parse(
        r#"
let pending = upstream_state { source = "../pending" }
let r = mock.test.resource { name = "r" }
fn f(x: Int) { "fixed" }
let generated = for item in pending.items {
    mock.test.resource { name = f(r.name) }
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);
    let schemas = function_ref_schema();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let error = validate_user_function_ref_types_with_bindings(&parsed, &bindings)
        .expect_err("deferred body function argument mismatch must be rejected");
    assert!(
        error.contains("expected Int") && error.contains("got String"),
        "{error}"
    );
}

#[test]
fn function_return_reference_uses_the_declared_return_type_and_matching_refs_pass() {
    let mismatch = crate::parser::parse(
        r#"
let r = mock.test.resource { name = "r" }
fn f(): Int { r.name }
let r2 = mock.test.resource { name = f() }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let schemas = function_ref_schema();
    let mismatch_bindings = crate::binding_index::BindingIndex::from_parsed(
        &mismatch,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let error = validate_user_function_ref_types_with_bindings(&mismatch, &mismatch_bindings)
        .expect_err("String function result passed to Int must be rejected");
    assert!(
        error.contains("expected Int") && error.contains("got String"),
        "{error}"
    );

    let matching = crate::parser::parse(
        r#"
let r = mock.test.resource { name = "r" }
fn f(): String { r.name }
let r2 = mock.test.resource { name = f() }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let matching_bindings = crate::binding_index::BindingIndex::from_parsed(
        &matching,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(validate_user_function_ref_types_with_bindings(&matching, &matching_bindings).is_ok());
}

#[test]
fn nested_user_function_call_uses_declared_return_type_as_argument_source() {
    fn parsed_with_nested_call(g_return: &str) -> ParsedFile {
        let mut parsed = crate::parser::parse(
            &format!("fn f(x: Int): String {{ 'ok' }}\nfn g(y: Int): {g_return} {{ y }}\n"),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("function declarations must parse");
        parsed.variables.insert(
            "result".to_string(),
            Value::Deferred(DeferredValue::ResolvedFunctionCall {
                function: std::sync::Arc::new(parsed.user_functions["f"].clone()),
                functions: std::sync::Arc::new(parsed.user_functions.clone()),
                args: vec![
                    Value::Deferred(DeferredValue::ResolvedFunctionCall {
                        function: std::sync::Arc::new(parsed.user_functions["g"].clone()),
                        functions: std::sync::Arc::new(parsed.user_functions.clone()),
                        args: vec![Value::Concrete(ConcreteValue::Int(1)).into()],
                        source_line: (0).into(),
                    })
                    .into(),
                ],
                source_line: (0).into(),
            }),
        );
        parsed
    }

    let schemas = SchemaRegistry::new();
    let mismatch = parsed_with_nested_call("String");
    let bindings = BindingIndex::from_parsed(
        &mismatch,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let error = validate_user_function_ref_types_with_bindings(&mismatch, &bindings)
        .expect_err("g(): String must not satisfy f(Int)");
    assert!(
        error.contains("function 'f': argument 1")
            && error.contains("expected Int")
            && error.contains("got String"),
        "{error}"
    );

    let matching = parsed_with_nested_call("Int");
    let bindings = BindingIndex::from_parsed(
        &matching,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(validate_user_function_ref_types_with_bindings(&matching, &bindings).is_ok());
}

/// Explicit String declarations for the legacy identity-relation fixtures.
fn string_refinement_fixture() -> (LoadedProviderContext, SchemaRegistry) {
    let mut schemas = SchemaRegistry::new();
    let mut known = HashSet::new();
    for (index, (owner, identity)) in [
        ("test", TypeIdentity::bare("Arn")),
        ("test", TypeIdentity::bare("AwsAccountId")),
        ("test", TypeIdentity::bare("IamRoleArn")),
        ("test", TypeIdentity::bare("IamPolicyArn")),
        ("test", TypeIdentity::bare("IamOidcProviderArn")),
        ("test", TypeIdentity::bare("KmsKeyArn")),
        ("test", TypeIdentity::bare("ResourceId")),
        ("test", TypeIdentity::bare("VpcId")),
        ("test", TypeIdentity::bare("SubnetId")),
        (
            "aws",
            TypeIdentity::from_schema_type("aws", "ec2.Vpc", "Id"),
        ),
        (
            "aws",
            TypeIdentity::from_schema_type("aws", "ec2.SecurityGroup", "Id"),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        schemas.insert(
            owner,
            PatternResourceSchema::new(format!("fixture{index}"))
                .attribute(
                    crate::schema::AttributeSchema::optional(
                        "value",
                        PatternAttributeType::refined_string(
                            Some(identity.clone()),
                            None,
                            None,
                            None,
                        ),
                    )
                    .build(),
                )
                .validate()
                .unwrap(),
        );
        known.insert(crate::parser::OwnedTypeIdentity::provider(
            owner,
            identity,
            crate::schema::RefinedScalarKind::String,
        ));
    }
    (context_with_accepting_custom_type_resolver(known), schemas)
}

/// Directional test adapter: `type_expr` is the source and the schema is the sink.
fn lifted_source_is_assignable_to_schema(
    type_expr: &TypeExpr,
    sink: &AttributeType,
    sink_defs: &std::collections::BTreeMap<String, AttributeType>,
) -> bool {
    let (context, _) = string_refinement_fixture();
    let resolved = resolve_type_expr(type_expr, &context).expect("fixture annotation must resolve");
    let source = lift_type_expr(&resolved).expect("fixture declaration must lift");
    crate::schema::TypeInSchema::schemaless(&source).is_assignable_to(
        (sink.in_schema(sink_defs)).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER),
        ),
    )
}

/// Directional test adapter for the opposite boundary role: a schema
/// attribute is the source and `type_expr` is the declared sink.
fn schema_source_is_assignable_to_lifted_sink(
    source: &AttributeType,
    source_defs: &std::collections::BTreeMap<String, AttributeType>,
    type_expr: &TypeExpr,
) -> bool {
    let (context, _) = string_refinement_fixture();
    let resolved = resolve_type_expr(type_expr, &context).expect("fixture annotation must resolve");
    let sink = lift_type_expr(&resolved).expect("fixture declaration must lift");
    source.in_schema(source_defs).is_assignable_to(
        (crate::schema::TypeInSchema::schemaless(&sink)).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER),
        ),
    )
}

fn loaded_context_for_test(mut context: ProviderContext) -> LoadedProviderContext {
    if !context.custom_types_loaded() {
        context.custom_types = crate::parser::ProviderCustomTypes::loaded(
            HashSet::new(),
            Box::new(|_target, _value| Ok(())),
        );
    }
    LoadedProviderContext::for_test(context)
}

fn context_with_iam_policy_arn_validator() -> LoadedProviderContext {
    let identity = TypeIdentity::bare("IamPolicyArn");
    loaded_context_for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                "test",
                identity.clone(),
                crate::schema::RefinedScalarKind::String,
            )]),
            Box::new(move |target, value| {
                if target.identity() != &identity {
                    return Ok(());
                }
                let GenericValue::Concrete(crate::resource::ConcreteValue::String(s)) = value
                else {
                    return Err(crate::parser::fixture_custom_type_rejection(
                        target,
                        "Expected string",
                    ));
                };
                let s = s.expose_plaintext();
                if s.starts_with("arn:aws:iam::") {
                    Ok(())
                } else {
                    Err(crate::parser::fixture_custom_type_rejection(
                        target,
                        format!("invalid IAM policy ARN: '{s}'"),
                    ))
                }
            }),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    })
}

fn context_with_known_custom_types(
    known: HashSet<crate::parser::OwnedTypeIdentity>,
) -> LoadedProviderContext {
    loaded_context_for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            known,
            Box::new(|target, _value| {
                let owner = match target {
                    crate::parser::CustomTypeValidationTarget::Annotation(owned) => owned
                        .provider_names()
                        .next()
                        .expect("fixture custom type has an owner"),
                    crate::parser::CustomTypeValidationTarget::Declaration { owner, .. } => owner,
                };
                Err(
                    crate::parser::CustomTypeValidationError::MissingDeclaration {
                        owner: owner.to_string(),
                        identity: target.identity().clone(),
                    },
                )
            }),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    })
}

fn context_with_accepting_custom_type_resolver(
    known: HashSet<crate::parser::OwnedTypeIdentity>,
) -> LoadedProviderContext {
    loaded_context_for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            known,
            Box::new(|_target, _value| Ok(())),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    })
}

fn iam_policy_arn_type_expr() -> TypeExpr {
    TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "test",
            TypeIdentity::bare("IamPolicyArn"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    }
}

fn refined_int_module_argument_report(annotation: &str, sink: TypeExpr) -> BindingUseReport {
    let identity = TypeIdentity::new(Some("test"), ["counter"], "Count");
    let provider_context = context_with_accepting_custom_type_resolver(HashSet::from([
        crate::parser::OwnedTypeIdentity::provider(
            "test",
            identity.clone(),
            crate::schema::RefinedScalarKind::Int,
        ),
    ]));
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        PatternResourceSchema::new("counter")
            .attribute(
                crate::schema::AttributeSchema::optional(
                    "count",
                    PatternAttributeType::refined_int(Some(identity), None),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse(
        &format!(
            "arguments {{ n: {annotation} }}\nlet inner = use {{ source = './inner' }}\ninner {{ m = n }}\n"
        ),
        &provider_context,
    )
    .unwrap();
    let signatures = HashMap::from([(
        "inner".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![crate::parser::ArgumentParameter {
                name: "m".to_string(),
                static_type: consumer_type(sink),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    check_bare_binding_uses(&ValidationTypeContext::new(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            &schemas,
            provider_context.as_provider_context(),
        )
        .judged(),
        &provider_context,
        &signatures,
    ))
}

#[test]
fn positive_int_module_argument_accepts_int_sink() {
    let report = refined_int_module_argument_report("PositiveInt", TypeExpr::Int);
    assert!(report.is_ready(), "{report:#?}");
}

#[test]
fn positive_int_module_argument_rejects_string_sink_statically() {
    let report = refined_int_module_argument_report("PositiveInt", TypeExpr::String);
    assert_eq!(report.findings().len(), 1, "{report:#?}");
    assert!(
        report.findings()[0]
            .error
            .to_string()
            .contains("expected String, got PositiveInt")
    );
}

#[test]
fn provider_int_module_argument_accepts_int_sink() {
    let report = refined_int_module_argument_report("test.counter.Count", TypeExpr::Int);
    assert!(report.is_ready(), "{report:#?}");
}

#[test]
fn provider_int_module_argument_rejects_string_sink_statically() {
    let report = refined_int_module_argument_report("test.counter.Count", TypeExpr::String);
    assert_eq!(report.findings().len(), 1, "{report:#?}");
    assert!(
        report.findings()[0]
            .error
            .to_string()
            .contains("expected String, got test.counter.Count")
    );
}

#[test]
fn resolve_type_expr_rejects_conflicting_owner_scalar_kinds() {
    use crate::parser::OwnedTypeIdentity;
    use crate::schema::RefinedScalarKind;
    use crate::static_type::StaticTypeResolutionError;

    for (identity, annotation) in [
        (
            TypeIdentity::bare("Count"),
            crate::parser::bare_type_for_test("count".into()),
        ),
        (
            TypeIdentity::from_dotted("test.counter.Count"),
            TypeExpr::DottedUnresolved(crate::parser::ResourceTypePath::new(
                "test",
                "counter.Count",
            )),
        ),
    ] {
        let context = context_with_known_custom_types(HashSet::from([
            OwnedTypeIdentity::provider("one", identity.clone(), RefinedScalarKind::Int),
            OwnedTypeIdentity::provider("two", identity, RefinedScalarKind::String),
        ]));
        assert!(matches!(
            resolve_type_expr(&annotation, &context),
            Err(StaticTypeResolutionError::ConflictingScalarKinds { .. })
        ));
        if matches!(annotation, TypeExpr::Simple(_)) {
            assert!(
                crate::parser::parse_type_expr_str(&annotation.to_string(), &context).is_none()
            );
        }
    }
}

#[test]
fn lift_schema_type_preserves_int_kind_without_registry() {
    let identity = TypeIdentity::new(Some("test"), ["counter"], "Count");
    let type_expr = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "test",
            identity.clone(),
            crate::schema::RefinedScalarKind::Int,
        )
        .unbounded_witness_for_test(),
    };
    let lifted =
        lift_type_expr(&type_expr).expect("an identified Int must lift without a schema registry");
    let expected = AttributeType::refined_int(Some(identity), None);
    assert!(
        crate::schema::TypeInSchema::schemaless(&lifted).is_assignable_to(
            (crate::schema::TypeInSchema::schemaless(&expected)).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )
        )
    );
    assert!(matches!(
        lifted.raw_shape(),
        crate::schema::RawShape::Int { .. }
    ));
}

#[test]
fn lift_unloaded_schema_nominals_preserves_identity_without_guessing_kind() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let nominal = crate::parser::parse_type_expr_str("KmsKeyArn", &context).unwrap();
    let same = crate::parser::parse_type_expr_str("KmsKeyArn", &context).unwrap();
    let different = crate::parser::parse_type_expr_str("Arn", &context).unwrap();
    let nominal = resolve_type_expr(&nominal, &context).unwrap();
    let same = resolve_type_expr(&same, &context).unwrap();
    let different = resolve_type_expr(&different, &context).unwrap();
    let source = lift_type_expr(&nominal).expect("unloaded nominal must retain its identity");
    let same = lift_type_expr(&same).unwrap();
    let different = lift_type_expr(&different).unwrap();
    let source = crate::schema::TypeInSchema::schemaless(&source);
    assert!(source.is_assignable_to(
        (crate::schema::TypeInSchema::schemaless(&same)).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER)
        )
    ));
    assert_eq!(
        source.assignment_to(
            (crate::schema::TypeInSchema::schemaless(&different)).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )
        ),
        crate::schema::TypeAssignability::Mismatch
    );
    for sink in [AttributeType::string(), AttributeType::int()] {
        assert_eq!(
            crate::schema::TypeInSchema::schemaless(&sink).assignment_to((source).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )),
            crate::schema::TypeAssignability::Mismatch
        );
        let assignment = source.assignment_to(
            (crate::schema::TypeInSchema::schemaless(&sink)).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER),
            ),
        );
        assert_eq!(
            assignment,
            crate::schema::TypeAssignability::Unchecked(
                crate::schema::TypeAssignmentDeferral::new(nominal.clone()),
            )
        );
        assert_eq!(
            assignment.require_loaded(),
            Err(crate::parser::CustomTypeValidationError::ProvidersNotLoaded)
        );
    }
}

#[test]
fn unloaded_schema_nominals_compare_whole_identity_and_round_trip() {
    let nominal = |identity: TypeIdentity| {
        crate::parser::resolve_custom_type_identity(
            &identity,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .unwrap()
        .unwrap()
    };
    let original = nominal(TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn"));
    let decoded: TypeExpr =
        serde_json::from_str(&serde_json::to_string(&original).unwrap()).unwrap();
    assert_eq!(decoded, original);
    let source = lift_type_expr(&original).unwrap();
    let source = crate::schema::TypeInSchema::schemaless(&source);
    for (identity, assignable) in [
        (TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn"), true),
        (
            TypeIdentity::new(Some("gcp"), ["iam", "Role"], "Arn"),
            false,
        ),
        (
            TypeIdentity::new(Some("aws"), ["acm", "Certificate"], "Arn"),
            false,
        ),
        (
            TypeIdentity::new(Some("aws"), Vec::<String>::new(), "Arn"),
            true,
        ),
        (TypeIdentity::bare("Arn"), true),
    ] {
        let sink = lift_type_expr(&nominal(identity)).unwrap();
        let result = source.assignment_to(
            (crate::schema::TypeInSchema::schemaless(&sink)).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER),
            ),
        );
        assert_eq!(
            result,
            if assignable {
                crate::schema::TypeAssignability::Assignable
            } else {
                crate::schema::TypeAssignability::Mismatch
            }
        );
    }
}

#[test]
fn unloaded_schema_nominals_use_identity_relation_with_loaded_types() {
    use crate::schema::{TypeAssignability, TypeInSchema};

    let identity = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
    let nominal = crate::parser::resolve_custom_type_identity(
        &identity,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .unwrap();
    let unloaded = lift_type_expr(&nominal).unwrap();
    let unloaded = TypeInSchema::schemaless(&unloaded);
    // No scalar kind is required to compare the same nominal identity.
    for loaded in [
        AttributeType::refined_string(Some(identity.clone()), None, None, None),
        AttributeType::refined_int(Some(identity.clone()), None),
        AttributeType::enum_(identity.clone(), None, Vec::new(), None, None),
    ] {
        let loaded = TypeInSchema::schemaless(&loaded);
        assert_eq!(
            unloaded.assignment_to((loaded).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )),
            TypeAssignability::Assignable
        );
        assert_eq!(
            loaded.assignment_to((unloaded).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )),
            TypeAssignability::Assignable
        );
    }
    let wider = AttributeType::refined_string(Some(TypeIdentity::bare("Arn")), None, None, None);
    let wider = TypeInSchema::schemaless(&wider);
    assert_eq!(
        unloaded.assignment_to((wider).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER)
        )),
        TypeAssignability::Assignable
    );
    assert_eq!(
        wider.assignment_to((unloaded).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER)
        )),
        TypeAssignability::Mismatch
    );
    let other = AttributeType::refined_int(Some(TypeIdentity::bare("Count")), None);
    let other = TypeInSchema::schemaless(&other);
    assert_eq!(
        unloaded.assignment_to((other).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER)
        )),
        TypeAssignability::Mismatch
    );
    assert_eq!(
        other.assignment_to((unloaded).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER)
        )),
        TypeAssignability::Mismatch
    );

    let nominal = crate::parser::resolve_custom_type_identity(
        &TypeIdentity::new(Some("test"), ["Counter"], "PositiveInt"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .unwrap();
    let unloaded = lift_type_expr(&nominal).unwrap();
    let unloaded = TypeInSchema::schemaless(&unloaded);
    let builtin =
        crate::schema::builtin_type_registry()[&TypeIdentity::bare("PositiveInt")].clone();
    let builtin = TypeInSchema::schemaless(&builtin);
    assert_eq!(
        unloaded.assignment_to((builtin).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER)
        )),
        TypeAssignability::Assignable
    );
    assert_eq!(
        builtin.assignment_to((unloaded).with_authority(
            crate::schema::assignment_catalog_without_edges_for_test(),
            crate::schema::SinkAuthority::Owner(&TEST_OWNER)
        )),
        TypeAssignability::Mismatch
    );
}

#[test]
fn unloaded_schema_nominals_defer_values_and_resolve_when_catalog_loads() {
    let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let parsed = crate::parser::parse(
        "arguments { count: Count }\nattributes { next: Int = (count + 1) * 2 }",
        &context,
    )
    .unwrap();
    validate_resource_ref_types_for_test(&parsed, &SchemaRegistry::new())
        .expect("unloaded operand kinds defer while retaining the Int result shape");
    let ty =
        resolve_type_expr(&crate::parser::bare_type_for_test("count".into()), &context).unwrap();
    let value: Value = Value::Concrete(ConcreteValue::Int(3));
    assert!(matches!(
        resolve_parameter_type_for_check(&ty, &context).unwrap(),
        Some(ResolvedParameterType::Value(TypeExpr::UnloadedSchemaType(
            _
        )))
    ));
    let error = typecheck::check_type_expr_value_with_context(
        &ty,
        &value,
        typecheck::TypecheckProviderContext::for_parser(&context),
        &crate::parser::retained::value_dependent_judgement_for_test(),
    )
    .require_provider_schemas()
    .unwrap_err();
    assert!(
        matches!(error, typecheck::TypeExprValueError::CustomTypeValidation { error, .. }
        if matches!(*error, crate::parser::CustomTypeValidationError::ProvidersNotLoaded))
    );
    let lifted = lift_type_expr(&ty).unwrap();
    assert!(matches!(
        crate::provider::canonicalize_custom_type_value(
            &lifted,
            &RuntimeValue::Concrete(RuntimeConcreteValue::Int(3))
        ),
        Err(crate::provider::CustomTypeCanonicalizationError::ProvidersNotLoaded)
    ));
    assert!(matches!(
        crate::protocol_convert::attribute_type_to_protocol(&lifted),
        Err(
            crate::protocol_convert::ProtocolConvertError::NonWireAttributeType {
                kind: "UnloadedSchemaType"
            }
        )
    ));
    let owned = crate::parser::OwnedTypeIdentity::provider(
        "test",
        TypeIdentity::bare("Count"),
        crate::schema::RefinedScalarKind::Int,
    );
    let loaded = ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([owned.clone()]),
            Box::new(|_, _| Ok(())),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    assert_eq!(
        resolve_type_expr(&ty, &loaded).unwrap(),
        TypeExpr::SchemaType {
            owned: owned.unbounded_witness_for_test()
        }
    );
    assert!(
        resolve_type_expr(
            &resolve_type_expr(&crate::parser::bare_type_for_test("typo".into()), &context)
                .unwrap(),
            &loaded,
        )
        .is_err()
    );
}

#[test]
fn lift_type_expr_preserves_every_builtin_scalar_kind() {
    for (kind, identity) in crate::schema::builtin_validator_registry().keys() {
        let expr =
            crate::parser::bare_type_for_test(crate::parser::pascal_to_snake(&identity.kind));
        let lifted = lift_type_expr(&expr).unwrap();
        let expected = match kind {
            crate::schema::RefinedScalarKind::String => AttributeType::string(),
            crate::schema::RefinedScalarKind::Int => AttributeType::int(),
            crate::schema::RefinedScalarKind::Float => AttributeType::float(),
        };
        assert!(
            crate::schema::TypeInSchema::schemaless(&lifted).is_assignable_to(
                (crate::schema::TypeInSchema::schemaless(&expected)).with_authority(
                    crate::schema::assignment_catalog_without_edges_for_test(),
                    crate::schema::SinkAuthority::Owner(&TEST_OWNER)
                )
            ),
            "{identity} must lift to its constructor's {kind} scalar kind"
        );
    }
}

#[test]
fn lift_type_expr_maps_schema_identity_directionally() {
    let vpc_id_expr = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", "ec2.Vpc", "Id"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let vpc_id = AttributeType::refined_string(
        Some(TypeIdentity::from_schema_type("aws", "ec2.Vpc", "Id")),
        None,
        None,
        None,
    );
    let security_group_id = AttributeType::refined_string(
        Some(TypeIdentity::from_schema_type(
            "aws",
            "ec2.SecurityGroup",
            "Id",
        )),
        None,
        None,
        None,
    );

    assert!(schema_source_is_assignable_to_lifted_sink(
        &vpc_id,
        crate::schema::empty_defs_for_schema_walks(),
        &vpc_id_expr,
    ));
    assert!(!schema_source_is_assignable_to_lifted_sink(
        &security_group_id,
        crate::schema::empty_defs_for_schema_walks(),
        &vpc_id_expr,
    ));
}

#[test]
fn lift_type_expr_recurses_and_only_unknown_remains_unchecked() {
    let structured = TypeExpr::List(Box::new(TypeExpr::Map(Box::new(TypeExpr::Union(vec![
        TypeExpr::String,
        TypeExpr::Int,
    ])))));
    let lifted = lift_type_expr(&structured).expect("container types must lift recursively");
    let expected = AttributeType::list(AttributeType::map(AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::int(),
    ])));
    assert!(
        crate::schema::TypeInSchema::schemaless(&lifted).is_assignable_to(
            (crate::schema::TypeInSchema::schemaless(&expected)).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )
        )
    );

    let resource_ref =
        crate::parser::ResourceTypePath::new("aws".to_string(), "ec2.Vpc".to_string());
    let handle = lift_type_expr(&TypeExpr::Ref(resource_ref.clone()))
        .expect("resolved resource paths lift to typed handles");
    assert!(
        !crate::schema::TypeInSchema::schemaless(&handle).is_assignable_to(
            (crate::schema::TypeInSchema::schemaless(&AttributeType::string())).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )
        )
    );
    assert!(
        !crate::schema::TypeInSchema::schemaless(&handle).is_assignable_to(
            (crate::schema::TypeInSchema::schemaless(&AttributeType::int())).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )
        )
    );
    assert!(
        lift_type_expr(&TypeExpr::DottedUnresolved(resource_ref)).is_none(),
        "an unresolved dotted declaration is not yet known to be a handle",
    );
    let literal = lift_type_expr(&TypeExpr::StringLiteral("x".to_string()))
        .expect("string literals lift to a closed string set");
    assert!(
        crate::schema::TypeInSchema::schemaless(&literal).is_assignable_to(
            (crate::schema::TypeInSchema::schemaless(&AttributeType::string())).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            )
        )
    );
}

#[test]
fn literal_union_lifts_directionally_for_schema_assignment() {
    let literals = TypeExpr::Union(vec![
        TypeExpr::StringLiteral("dev".to_string()),
        TypeExpr::StringLiteral("prod".to_string()),
    ]);
    let identified_string = AttributeType::refined_string(
        Some(TypeIdentity::bare("EnvironmentName")),
        None,
        None,
        None,
    );

    assert!(!lifted_source_is_assignable_to_schema(
        &literals,
        &AttributeType::int(),
        crate::schema::empty_defs_for_schema_walks(),
    ));
    assert!(!lifted_source_is_assignable_to_schema(
        &literals,
        &identified_string,
        crate::schema::empty_defs_for_schema_walks(),
    ));
    assert!(lifted_source_is_assignable_to_schema(
        &literals,
        &AttributeType::string(),
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn no_bindings_no_warnings() {
    let parsed = empty_parsed();
    assert!(check_unused_bindings_in_file(&parsed).is_empty());
}

#[test]
fn used_binding_no_warning() {
    let mut parsed = empty_parsed();

    // Resource with a binding
    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    // Resource that references the binding
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet", None).with_attribute(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    parsed.resources.push(subnet); // allow: direct — fixture test inspection

    assert!(check_unused_bindings_in_file(&parsed).is_empty());
}

#[test]
fn unused_binding_warns() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let unused = check_unused_bindings_in_file(&parsed);
    assert_eq!(unused, vec!["vpc"]);
}

#[test]
fn import_target_counts_as_binding_use_and_other_binding_still_warns() {
    let src = r#"
import {
  to = mock.test.resource 'imported'
  id = 'existing-1'
}

let imported = mock.test.resource { name = "imported" }
let orphan = mock.test.resource { name = "orphan" }
"#;

    assert_eq!(
        check_unused_bindings(&authored_binding_view(src)),
        vec!["orphan"]
    );
}

#[test]
fn state_block_targets_only_count_managed_resources_as_used() {
    let mut parsed = empty_parsed();
    let managed =
        Resource::with_provider("mock", "test.resource", "managed", None).with_binding("managed");
    parsed.resources.push(managed); // allow: direct — fixture test inspection
    parsed.data_sources.push(
        DataSource::with_provider("mock", "test.resource", "read_target", None)
            .with_binding("read_target"),
    );
    parsed.state_blocks.push(crate::parser::StateBlock::Import {
        to: crate::parser::StateBlockAddress::new("mock", "test.resource", "managed"),
        to_span: None,
        id: Value::Concrete(ConcreteValue::String("existing-1".into())),
    });
    parsed.state_blocks.push(crate::parser::StateBlock::Import {
        to: crate::parser::StateBlockAddress::new("mock", "test.resource", "read_target"),
        to_span: None,
        id: Value::Concrete(ConcreteValue::String("existing-2".into())),
    });

    assert_eq!(check_unused_bindings_in_file(&parsed), vec!["read_target"]);
}

#[test]
fn moved_target_counts_as_binding_use_but_source_does_not() {
    let src = r#"
moved {
  from = mock.test.resource 'old'
  to = mock.test.resource 'renamed'
}

removed {
  from = mock.test.resource 'gone'
}

let old = mock.test.resource { name = "old" }
let renamed = mock.test.resource { name = "renamed" }
let gone = mock.test.resource { name = "gone" }
let orphan = mock.test.resource { name = "orphan" }
"#;

    assert_eq!(
        check_unused_bindings(&authored_binding_view(src)),
        vec!["old", "gone", "orphan"]
    );
}

#[test]
fn binding_referenced_only_in_import_id_is_not_unused() {
    let src = r#"
let imported = mock.test.resource { name = "imported" }
let identifier_source = mock.test.resource { name = "identifier" }

mock.test.resource { name = imported.name }

import {
  to = mock.test.resource 'imported'
  id = "${identifier_source.id}|existing"
}
"#;

    assert!(check_unused_bindings(&authored_binding_view(src)).is_empty());
}

#[test]
fn dotted_string_literal_does_not_count_as_binding_use() {
    let mut parsed = empty_parsed();

    let publish =
        Resource::with_provider("test", "r.Source", "publish", None).with_binding("publish");
    parsed.resources.push(publish); // allow: direct — fixture test inspection

    let target = Resource::with_provider("test", "r.Target", "target", None).with_attribute(
        "x",
        Value::Concrete(ConcreteValue::String("publish.example.com".into())),
    );
    parsed.resources.push(target); // allow: direct — fixture test inspection

    let unused = check_unused_bindings_in_file(&parsed);
    assert_eq!(unused, vec!["publish"]);
}

#[test]
fn anonymous_resource_no_warning() {
    let mut parsed = empty_parsed();

    // Anonymous resource (no typed binding)
    let bucket = Resource::with_provider("awscc", "s3.Bucket", "my-bucket", None).with_attribute(
        "bucket_name",
        Value::Concrete(ConcreteValue::String("my-bucket".into())),
    );
    parsed.resources.push(bucket); // allow: direct — fixture test inspection

    assert!(check_unused_bindings_in_file(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_nested_value() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    // Reference inside a Map inside a List
    let mut map = IndexMap::new();
    map.insert(
        "vpc_id".to_string(),
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    let sg = Resource::with_provider("awscc", "ec2.SecurityGroup", "web-sg", None).with_attribute(
        "tags",
        Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::Map(map),
        )])),
    );
    parsed.resources.push(sg); // allow: direct — fixture test inspection

    assert!(check_unused_bindings_in_file(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_interpolation_is_not_unused() {
    let src = r#"
        let role = aws.iam.Role {
            role_name = "r"
            assume_role_policy_document = "{}"
        }
        let _ = aws.s3.Bucket {
            bucket_name = "acct-${role.arn}"
        }
    "#;
    assert!(check_unused_bindings(&authored_binding_view(src)).is_empty());
}

#[test]
fn binding_referenced_in_function_call_arg_is_not_unused() {
    let src = r#"
        let role = aws.iam.Role {
            role_name = "r"
            assume_role_policy_document = "{}"
        }
        let _ = aws.s3.Bucket {
            bucket_name = upper(role.arn)
        }
    "#;
    assert!(check_unused_bindings(&authored_binding_view(src)).is_empty());
}

#[test]
fn binding_referenced_in_secret_builtin_function_call_is_not_unused() {
    let src = r#"
        let role = aws.iam.Role {
            role_name = "r"
            assume_role_policy_document = "{}"
        }
        let _ = aws.s3.Bucket {
            bucket_name = secret(role.arn)
        }
    "#;
    // `secret(role.arn)` stays deferred as a FunctionCall while its arg is
    // unresolved, so this source-level fixture pins function-call recursion.
    assert!(check_unused_bindings(&authored_binding_view(src)).is_empty());
}

#[test]
fn binding_referenced_only_by_deferred_for_iterable_is_not_unused() {
    let src = r#"
        let cert = aws.acm.Certificate {
            domain_name       = "registry.example.com"
            validation_method = "DNS"
        }

        let zone = aws.route53.HostedZone {
            name = "example.com"
        }

        let validation_records = for opt in cert.domain_validation_options {
            aws.route53.RecordSet {
                hosted_zone_id   = zone.id
                name             = opt.resource_record.name
                type             = opt.resource_record.type
                ttl              = 60
                resource_records = [opt.resource_record.value]
            }
        }
    "#;
    let parsed = crate::parser::parse(
        src,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);
    assert!(check_unused_bindings(&authored_binding_view(src)).is_empty());
}

#[test]
fn seeded_bare_structural_binding_is_a_typed_pending_slot() {
    let src = r#"
        let _ = aws.s3.Bucket {
            bucket_name = role
        }
        let role = aws.iam.Role {
            role_name = "r"
            assume_role_policy_document = "{}"
        }
    "#;
    let handle = crate::eval_value::UnexpandedStructuralHandle::new(
        crate::eval_value::UnexpandedStructuralHandleKind::Resource(
            crate::parser::ResourceTypePath::new("aws", "iam.Role"),
        ),
        "role",
    );
    let parsed = crate::parser::parse_with_seeded_bindings(
        src,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &[crate::parser::BindingSeed::typed_structural(
            "role", &handle,
        )],
    )
    .expect("a seeded structural handle reaches typed validation");

    assert!(
        parsed.resources[0].attributes.get("bucket_name").is_none(),
        "a pending structural leaf must not manufacture a runtime Value",
    );
    let use_site = parsed
        .bare_binding_uses()
        .uses()
        .find(|use_site| use_site.name == "role")
        .expect("seeded structural use");
    assert!(matches!(
        use_site.binding,
        crate::parser::BareBindingKind::Structural { .. }
    ));
    assert_eq!(use_site.path.root_name(), "bucket_name");
    assert_eq!(parsed.bare_binding_uses().pending_slots().count(), 1);
}

#[test]
fn binding_use_finding_display_is_context_only_and_error_is_its_source() {
    let src = r#"
        let _ = aws.s3.Bucket {
            bucket_name = role
        }
    "#;
    let handle = crate::eval_value::UnexpandedStructuralHandle::new(
        crate::eval_value::UnexpandedStructuralHandleKind::Resource(
            crate::parser::ResourceTypePath::new("aws", "iam.Role"),
        ),
        "role",
    );
    let parsed = crate::parser::parse_with_seeded_bindings(
        src,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &[crate::parser::BindingSeed::typed_structural(
            "role", &handle,
        )],
    )
    .expect("a seeded structural handle reaches typed validation");
    let schemas = SchemaRegistry::new();
    let provider_context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let module_signatures = crate::module_resolver::ResolvedModuleSignatures::new();
    let judged_configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &provider_context);
    let context = ValidationTypeContext::new_providerless(
        judged_configuration.judged(),
        &provider_context,
        &module_signatures,
    );
    let report = check_bare_binding_uses(&context);
    let finding = report.findings().first().expect("binding-use finding");
    let detail = finding.error.to_string();

    assert!(finding.to_string().contains("at line 3"));
    assert!(
        !finding.to_string().contains(&detail),
        "Display must not duplicate its Error::source(): {finding}",
    );
    let source = std::error::Error::source(finding).expect("typed resolution source");
    assert_eq!(source.to_string(), detail);
    assert!(
        source.source().is_none(),
        "a terminal resolution diagnostic must not repeat its own Display",
    );
}

#[cfg(unix)]
fn symlink_parent_source(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let private = temp.path().join("private");
    std::fs::create_dir_all(project.join("bad")).unwrap();
    std::fs::create_dir_all(private.join("nested")).unwrap();
    std::fs::create_dir(private.join("bad")).unwrap();
    std::fs::write(project.join("bad/main.crn"), "wrong file").unwrap();
    std::fs::write(private.join("bad/main.crn"), contents).unwrap();
    std::os::unix::fs::symlink(private.join("nested"), project.join("jump")).unwrap();
    let source = project.join("jump/../bad/main.crn");
    assert_eq!(std::fs::read_to_string(&source).unwrap(), contents);
    (temp, source)
}

#[cfg(unix)]
#[test]
fn binding_diagnostics_preserve_symlink_parent_source_spelling() {
    let (_temp, path) = symlink_parent_source(
        "arguments { value: Int }\nlet item = mock.test.resource { name = value }\n",
    );
    let provider = LoadedProviderContext::empty_for_test();
    let parsed =
        crate::parser::parse_file(&std::fs::read_to_string(&path).unwrap(), &path, &provider)
            .unwrap();
    let schemas = function_ref_schema();
    let signatures = crate::module_resolver::ResolvedModuleSignatures::new();
    let report = check_bare_binding_uses(&ValidationTypeContext::new(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            &schemas,
            provider.as_provider_context(),
        )
        .judged(),
        &provider,
        &signatures,
    ));
    assert_eq!(report.findings().len(), 1, "{report:?}");
    let finding = &report.findings()[0];
    assert_eq!(finding.use_site.source_file.path(), path);
    assert_eq!(finding.to_string(), format!("{} at line 2", path.display()));
    assert_eq!(
        report.to_string(),
        format!("{}: {} at line 2", path.display(), finding.error)
    );
    let nested = ModuleCallFindingMessage::NestedResourceBindingUse(Box::new(finding.clone()));
    assert_eq!(nested.to_string(), report.to_string());
}

#[cfg(unix)]
#[test]
fn module_call_diagnostics_preserve_symlink_parent_source_spelling() {
    let (_temp, path) = symlink_parent_source(
        "let child = use { source = './child' }\nchild { surprise = true }\n",
    );
    let provider = LoadedProviderContext::empty_for_test();
    let parsed =
        crate::parser::parse_file(&std::fs::read_to_string(&path).unwrap(), &path, &provider)
            .unwrap();
    let signatures = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: Vec::new(),
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    let findings = module_call_findings_for_test(&parsed, &signatures, &provider);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].anchor().source_file.path(), path);
    assert_eq!(
        findings[0].to_string(),
        format!("{} at line 2", path.display())
    );
    assert_eq!(module_use_anchor(&parsed.uses[0]).source_file.path(), path);
    let diagnostic = TypeResolutionDiagnostic::description(
        "module declaration".to_string(),
        crate::static_type::StaticTypeResolutionError::UnknownCustomType {
            name: "Missing".to_string(),
        }
        .into(),
        parser_test_location(&path),
    );
    let finding = ModuleCallFinding::from_type_resolution(diagnostic);
    assert_eq!(finding.anchor().source_file.path(), path);
}

#[test]
fn parser_side_deferred_resource_keeps_bare_binding_sink_type() {
    let parsed = crate::parser::parse(
        r#"
arguments {
    items: list(String)
    value: Int
}
let generated = for item in items {
    mock.test.resource { name = value }
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    assert_eq!(parsed.parsed_deferred_for_expressions().len(), 1);
    let schemas = function_ref_schema();
    let module_signatures = crate::module_resolver::ResolvedModuleSignatures::new();
    let provider_context = LoadedProviderContext::empty_for_test();
    let judged_configuration = crate::config_loader::load_buffer_configuration(
        &parsed,
        &schemas,
        provider_context.as_provider_context(),
    );
    let context = ValidationTypeContext::new(
        judged_configuration.judged(),
        &provider_context,
        &module_signatures,
    );

    let report = check_bare_binding_uses(&context);
    assert!(
        !report.is_ready(),
        "expected typed sink mismatch: {report:#?}"
    );
    let rendered = format!("{:#?}", report.findings());
    assert!(
        rendered.contains("String") && rendered.contains("Int"),
        "{rendered}"
    );
}

#[test]
fn binding_referenced_in_module_call() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let mut args = HashMap::new();
    args.insert(
        "vpc_id".to_string(),
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    parsed.module_calls.push(ModuleCall::test(
        "web_tier".to_string(),
        None,
        module_arguments(args),
    ));

    assert!(check_unused_bindings_in_file(&parsed).is_empty());
}

#[test]
fn multiple_bindings_some_unused() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    let sg = Resource::with_provider("awscc", "ec2.SecurityGroup", "web-sg", None)
        .with_binding("web_sg");
    parsed.resources.push(sg); // allow: direct — fixture test inspection

    // Only vpc is referenced
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet", None).with_attribute(
        "vpc_id",
        Value::resource_ref("vpc".to_string(), "vpc_id".to_string(), vec![]),
    );
    parsed.resources.push(subnet); // allow: direct — fixture test inspection

    let unused = check_unused_bindings_in_file(&parsed);
    assert_eq!(unused, vec!["web_sg"]);
}

#[test]
fn binding_referenced_in_attributes_not_warned() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter::dummy(
            "vpc_id".to_string(),
            Some(TypeExpr::String),
            Some(Value::resource_ref(
                "vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ));

    assert!(check_unused_bindings_in_file(&parsed).is_empty());
}

#[test]
fn binding_referenced_in_exports_not_warned() {
    let mut parsed = empty_parsed();

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    parsed.resources.push(vpc); // allow: direct — fixture test inspection

    parsed
        .export_params
        .push(crate::parser::ExportParameter::dummy(
            "vpc_id".to_string(),
            Some(TypeExpr::String),
            Some(Value::resource_ref(
                "vpc".to_string(),
                "vpc_id".to_string(),
                vec![],
            )),
        ));

    assert!(
        check_unused_bindings_in_file(&parsed).is_empty(),
        "binding referenced in exports should not be warned"
    );
}

#[test]
fn igw_route_crn_unused_detection() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
}

let igw = awscc.ec2.internet_gateway {
}

let igw_attachment = awscc.ec2.vpc_gateway_attachment {
  vpc_id              = vpc.vpc_id
  internet_gateway_id = igw.internet_gateway_id
}

let rt = awscc.ec2.RouteTable {
  vpc_id = vpc.vpc_id
}

let route = awscc.ec2.route {
  route_table_id         = rt.route_table_id
  destination_cidr_block = "0.0.0.0/0"
  gateway_id             = igw_attachment.internet_gateway_id
}
"#;
    let parsed = crate::parser::parse(
        input,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    // Check the route resource's gateway_id is a ResourceRef to igw_attachment
    let route = parsed
        .resources
        .iter()
        .find(|r| r.id.identity_str().expect("resolved identity") == "route")
        .unwrap();
    let gateway_id = route.get_attr("gateway_id").unwrap();
    match gateway_id {
        Value::Deferred(DeferredValue::ResourceRef { path }) => {
            assert_eq!(path.binding(), "igw_attachment");
            assert_eq!(path.attribute(), "internet_gateway_id");
        }
        other => panic!("Expected ResourceRef, got {:?}", other),
    }

    let unused = check_unused_bindings(&authored_binding_view(input));
    // igw_attachment is referenced by route, so should NOT be unused
    // route is the last resource and not referenced, so IS unused
    assert!(
        !unused.contains(&"igw_attachment".to_string()),
        "igw_attachment should not be unused"
    );
    assert_eq!(unused, vec!["route"]);
}

#[test]
fn if_expression_binding_not_warned() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let enabled = true

let vpc = if enabled {
  awscc.ec2.Vpc {
cidr_block = "10.0.0.0/16"
  }
}
"#;
    let parsed = crate::parser::parse(
        input,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert!(
        parsed.structural_bindings.contains("vpc"),
        "vpc should be in structural_bindings"
    );
    let unused = check_unused_bindings(&authored_binding_view(input));
    assert!(
        !unused.contains(&"vpc".to_string()),
        "if-expression binding should not be warned as unused"
    );
}

#[test]
fn for_expression_binding_not_warned() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpcs = for (i, env) in ["dev", "stg"] {
  awscc.ec2.Vpc {
cidr_block = cidr_subnet("10.0.0.0/8", 8, i)
  }
}
"#;
    let parsed = crate::parser::parse(
        input,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert!(
        parsed.structural_bindings.contains("vpcs"),
        "vpcs should be in structural_bindings"
    );
    let unused = check_unused_bindings(&authored_binding_view(input));
    assert!(
        unused.is_empty(),
        "for-expression bindings should not be warned as unused, got: {:?}",
        unused
    );
}

#[test]
fn bindings_in_both_deferred_for_coalesce_operands_are_not_warned() {
    let input = r#"
let primary = test.r.source {}
let fallback = test.r.source {}

for item in (primary.items ?? fallback.items) {
  test.r.child { name = item }
}
"#;
    let unused = check_unused_bindings(&authored_binding_view(input));

    assert!(
        !unused.contains(&"primary".to_string()),
        "coalesce left dependency must count as used: {unused:?}",
    );
    assert!(
        !unused.contains(&"fallback".to_string()),
        "coalesce default dependency must count as used: {unused:?}",
    );
}

#[test]
fn read_expression_binding_not_warned() {
    let input = r#"
provider aws {
  region = aws.Region.ap_northeast_1
}

let caller = read aws.sts.caller_identity {}
"#;
    let parsed = crate::parser::parse(
        input,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    assert!(
        parsed.structural_bindings.contains("caller"),
        "caller should be in structural_bindings"
    );
    let unused = check_unused_bindings(&authored_binding_view(input));
    assert!(
        !unused.contains(&"caller".to_string()),
        "read-expression binding should not be warned as unused"
    );
}

#[test]
fn genuinely_unused_binding_still_warns() {
    let input = r#"
provider awscc {
  region = awscc.Region.ap_northeast_1
}

let vpc = awscc.ec2.Vpc {
  cidr_block = "10.0.0.0/16"
}
"#;
    let unused = check_unused_bindings(&authored_binding_view(input));
    assert_eq!(
        unused,
        vec!["vpc"],
        "genuinely unused binding should still be warned"
    );
}

#[test]
fn block_local_shadow_use_does_not_mark_root_binding_used() {
    let input = r#"
        let shared = mock.test.Source {
            name = "root"
        }
        mock.test.Consumer {
            let shared = "local"
            name = shared
        }
    "#;

    assert_eq!(
        check_unused_bindings(&authored_binding_view(input)),
        vec!["shared"],
        "the bare use resolves to the block-local declaration, not the root resource",
    );
}

#[test]
fn module_internal_use_does_not_mark_same_named_root_binding_used() {
    let fixture = tempfile::tempdir().expect("module scope fixture");
    let module = fixture.path().join("modules/inner");
    std::fs::create_dir_all(&module).expect("create module directory");
    std::fs::write(
        fixture.path().join("main.crn"),
        r#"let shared = mock.test.Source { name = "root" }
let inner_module = use { source = "./modules/inner" }
inner_module {}
"#,
    )
    .expect("write root fixture");
    std::fs::write(
        module.join("main.crn"),
        r#"let shared = "module-local"
attributes { result: String = "ok" }
mock.test.Consumer { name = shared }
"#,
    )
    .expect("write module fixture");

    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    let parsed = crate::config_loader::parse_directory(
        &crate::paths::ResolvedPath::new(fixture.path()).unwrap(),
        &config,
    )
    .expect("load module fixture");
    let parsed = crate::validation::deferred_types::with_session_for_test(&config, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed,
            &crate::paths::ResolvedPath::new(fixture.path()).unwrap(),
            session,
            &schemas,
        )
        .expect("expand nested module")
    });

    assert_eq!(
        check_unused_bindings_in_resolved_file(&parsed),
        vec!["shared"],
        "module-local provenance must not count as a use of the root binding",
    );
}

#[test]
fn schema_less_resource_binding_is_conservatively_identity_required() {
    let input = r#"
provider foo {}

let a = foo.x.Y {}
"#;
    assert!(
        check_unused_bindings_with_identity_requirements(
            &authored_binding_view(input),
            &SchemaRegistry::new(),
        )
        .is_empty(),
        "a schema-less resource cannot safely discard its explicit identity binding",
    );
}

#[test]
fn binding_used_inside_for_body_is_not_flagged_as_unused() {
    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        let vpc = test.r.res { name = "v" }
        for _, id in orgs.accounts {
            test.r.res {
                name = vpc.name
            }
        }
    "#;
    let unused = check_unused_bindings(&authored_binding_view(src));
    assert!(
        !unused.iter().any(|b| b == "vpc"),
        "`vpc` is referenced inside the for body, got: {unused:?}"
    );
}

/// Helper to create a simple ResourceSchema with given attributes.
fn make_schema<R: crate::schema::PatternForm>(
    resource_type: &str,
    attrs: Vec<(&str, crate::schema::PatternAttributeType<R>)>,
) -> crate::schema::PatternResourceSchema<R> {
    attrs.into_iter().fold(
        PatternResourceSchema::new(resource_type),
        |schema, (name, attr_type)| {
            schema.attribute(PatternAttributeSchema::optional(name, attr_type).build())
        },
    )
}

fn make_composition(binding: &str, attributes: &[&str]) -> LiveComposition {
    let call = CompositionCall {
        id: crate::parser::ModuleCallId::test_dummy_in(format!("{binding}.crn")),
        module_name: "test_module".to_string(),
        binding: Some(binding.into()),
        instance: binding.to_string(),
        module_source: None,
        module_directory: None,
        loop_origin: None,
    };
    LiveComposition {
        id: crate::resource::ResolvedResourceId::with_identity(
            "_virtual",
            crate::resource::ResourceIdentity::try_from(binding.to_string()).unwrap(),
        ),
        signature: LiveCompositionSignature {
            arguments: IndexMap::new(),
            attributes: attributes
                .iter()
                .map(|name| {
                    (
                        (*name).to_string(),
                        LiveCompositionAttribute::from_value(
                            Value::Concrete(ConcreteValue::String(format!("{name}-value").into())),
                            None,
                        ),
                    )
                })
                .collect(),
            pending_constraints: Vec::new(),
        },
        binding: Some(binding.into()),
        dependency_bindings: Default::default(),
        module_name: "test_module".to_string(),
        instance: binding.to_string(),
        call_path: CompositionCallPath::direct(call),
        quoted_string_attrs: Default::default(),
        declaration: Default::default(),
    }
}

fn set_composition_module(composition: &mut LiveComposition, module_name: &str) {
    composition.module_name = module_name.to_string();
    let call = CompositionCall {
        id: composition.call_path.immediate().id.clone(),
        module_name: module_name.to_string(),
        binding: composition.binding.clone(),
        instance: composition.instance.clone(),
        module_source: None,
        module_directory: None,
        loop_origin: None,
    };
    composition.call_path = CompositionCallPath::direct(call);
}

#[test]
fn resource_ref_validation_checks_composition_attribute_membership_only() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "test.Consumer",
            vec![("target_group_arn", PatternAttributeType::bool())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut parsed = crate::parser::parse(
        "let valid = awscc.test.Consumer { target_group_arn = instance.target_group_arn.runtime_field }\nlet invalid = awscc.test.Consumer { target_group_arn = instance.target_group_ar }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let mut composition = make_composition("instance", &["target_group_arn"]);
    let module = crate::parser::parse(
        "attributes { target_group_arn = 'value' }",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    composition.declaration = crate::parser::DeclarationProvenance::for_composition(
        composition
            .call_path
            .immediate()
            .id
            .declaration_location()
            .clone(),
        module.parser_binding_metadata.syntax().declarations(),
    );
    parsed.compositions.push(composition);

    let err = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert_eq!(
        err.lines().count(),
        1,
        "declared composition attrs skip schema type/narrowing; only the miss should fail: {err}",
    );
    assert!(
        err.contains("unknown attribute 'target_group_ar' on 'instance'"),
        "expected composition membership error, got: {err}",
    );
}

#[test]
fn unknown_binding_reference_reports_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.Subnet",
            vec![("vpc_id", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut parsed = crate::parser::parse(
        "let subnet = awscc.ec2.Subnet { vpc_id = vpc.vpc_id }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("reference fixture must parse");

    // Preserve main's fixture identity while obtaining provenance from parsing.
    parsed.resources[0].id.set_identity("web-subnet".into());

    let result = validate_resource_ref_types_for_test(&parsed, &schemas);
    assert_eq!(
        result.unwrap_err(),
        "awscc.ec2.Subnet.web-subnet: unknown binding 'vpc' in reference vpc.vpc_id"
    );
}

#[test]
fn unknown_attribute_reference_reports_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.Vpc",
            vec![("cidr_block", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.Subnet",
            vec![("vpc_id", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut parsed = crate::parser::parse(
        "let vpc = awscc.ec2.Vpc { cidr_block = '10.0.0.0/16' }\nlet subnet = awscc.ec2.Subnet { vpc_id = vpc.nonexistent_attr }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    // Binding names and resource identities need not be the same.
    parsed.resources[0].id.set_identity("main-vpc".into());
    parsed.resources[1].id.set_identity("web-subnet".into());

    let result = validate_resource_ref_types_for_test(&parsed, &schemas);
    assert_eq!(
        result.unwrap_err(),
        "awscc.ec2.Subnet.web-subnet: unknown attribute 'nonexistent_attr' on 'vpc' in reference vpc.nonexistent_attr"
    );
}

fn postfix_access_validation_fixture(
    value: Value,
    sink: crate::schema::AttributeType,
) -> (ParsedFile, SchemaRegistry) {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (PatternResourceSchema::new("test.Queue")
            .attribute(
                PatternAttributeSchema::optional("identifier", PatternAttributeType::string())
                    .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "rules",
                    PatternAttributeType::list(PatternAttributeType::struct_(
                        "Rule".to_string(),
                        vec![
                            PatternStructField::optional("name", PatternAttributeType::string())
                                .build(),
                        ],
                    )),
                )
                .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "mock",
        (PatternResourceSchema::new("test.Consumer")
            .attribute(PatternAttributeSchema::optional("value", sink).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    // Exercise the checker on authored syntax, including the provenance of
    // references inside computed accesses.
    let parsed = crate::parser::parse(
        &format!(
            "let q = mock.test.Queue {{ identifier = 'a/b' rules = [] }}\nlet consumer = mock.test.Consumer {{ value = {} }}\n",
            crate::value::format_value(&value),
        ),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("postfix fixture must parse");
    (parsed, schemas)
}

#[test]
fn postfix_and_path_access_report_the_same_unknown_struct_field() {
    use crate::resource::{
        AccessMissPolicy, AccessPath, PathSegment, Subscript, apply_postfix_access,
    };

    let segments = vec![
        PathSegment::Subscript {
            index: Subscript::Int { index: 0 },
        },
        PathSegment::Field {
            name: "nope".to_string(),
        },
    ];
    let path_value = Value::Deferred(DeferredValue::ResourceRef {
        path: AccessPath::with_segments("q", "rules", segments.clone()),
    });
    let postfix_value =
        segments
            .into_iter()
            .fold(Value::resource_ref("q", "rules", vec![]), |base, step| {
                apply_postfix_access(base, step, 1, AccessMissPolicy::Error)
                    .expect("reference access canonicalizes")
            });

    assert_eq!(postfix_value, path_value);

    let (path_parsed, schemas) =
        postfix_access_validation_fixture(path_value, PatternAttributeType::string());
    let path_error = validate_resource_ref_types_for_test(&path_parsed, &schemas)
        .expect_err("path form must reject the unknown field");
    let (postfix_parsed, schemas) =
        postfix_access_validation_fixture(postfix_value, PatternAttributeType::string());
    let postfix_error = validate_resource_ref_types_for_test(&postfix_parsed, &schemas)
        .expect_err("postfix form must reject the same unknown field");

    assert_eq!(postfix_error, path_error);
    assert!(postfix_error.contains("unknown field 'nope'"));
}

#[test]
fn parenthesized_for_address_reports_the_same_unknown_field_as_direct_path() {
    let parsed = crate::parser::parse(
        r#"
            let roles = for n in [0] {
                mock.test.Thing { name = "role" }
            }
            mock.test.Consumer {
                direct  = roles[0].nope
                grouped = (roles[0]).nope
            }
        "#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("both reference spellings parse");
    let consumer = parsed
        .resources
        .iter()
        .find(|resource| resource.id.resource_type == "test.Consumer")
        .expect("consumer");
    assert_eq!(consumer.get_attr("direct"), consumer.get_attr("grouped"));

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (PatternResourceSchema::new("test.Thing")
            .attribute(
                PatternAttributeSchema::optional("name", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("identifier", PatternAttributeType::string())
                    .build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "mock",
        (PatternResourceSchema::new("test.Consumer")
            .attribute(
                PatternAttributeSchema::optional("direct", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("grouped", PatternAttributeType::string()).build(),
            ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("unknown field must be rejected for both spellings");
    assert_eq!(
        error.matches("unknown attribute 'nope'").count(),
        2,
        "{error}"
    );
    assert!(error.contains("roles[0].nope"), "{error}");
}

#[test]
fn postfix_access_rejects_steps_that_do_not_fit_known_base_types() {
    use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

    let cases = [
        apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String("/".into())).into(),
                    Value::resource_ref("q", "identifier", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Str { key: "bad".into() },
            },
            1,
            AccessMissPolicy::Error,
        )
        .expect("computed access remains deferred"),
        apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::upper,
                args: vec![Value::resource_ref("q", "identifier", vec![]).into()],
                source_line: (0).into(),
            }),
            PathSegment::Field {
                name: "foo".to_string(),
            },
            1,
            AccessMissPolicy::Error,
        )
        .expect("computed access remains deferred"),
    ];

    for value in cases {
        let (parsed, schemas) =
            postfix_access_validation_fixture(value, PatternAttributeType::string());
        let error = validate_resource_ref_types_for_test(&parsed, &schemas)
            .expect_err("a known base type must reject an incompatible access step");
        assert!(
            error.contains("cannot apply") || error.contains("does not fit"),
            "unexpected access-type error: {error}"
        );
    }
}

#[test]
fn postfix_function_result_type_is_checked_against_resource_attribute_type() {
    use crate::resource::{AccessMissPolicy, PathSegment, Subscript, apply_postfix_access};

    let split_element = || {
        apply_postfix_access(
            Value::Deferred(DeferredValue::FunctionCall {
                name: crate::builtins::BuiltinFunction::split,
                args: vec![
                    Value::Concrete(ConcreteValue::String("/".into())).into(),
                    Value::resource_ref("q", "identifier", vec![]).into(),
                ],
                source_line: (0).into(),
            }),
            PathSegment::Subscript {
                index: Subscript::Int { index: 0 },
            },
            1,
            AccessMissPolicy::Error,
        )
        .expect("computed access remains deferred")
    };

    let (invalid, schemas) =
        postfix_access_validation_fixture(split_element(), PatternAttributeType::int());
    let error = validate_resource_ref_types_for_test(&invalid, &schemas)
        .expect_err("split()[0] is String, not Int");
    assert!(
        error.contains("expected Int") && error.contains("String"),
        "{error}"
    );

    let (valid, schemas) =
        postfix_access_validation_fixture(split_element(), PatternAttributeType::string());
    validate_resource_ref_types_for_test(&valid, &schemas)
        .expect("split()[0] is assignable to String");
}

#[test]
fn secret_preserves_its_argument_type_through_postfix_access() {
    let (_, schemas) = postfix_access_validation_fixture(
        Value::Concrete(ConcreteValue::String("unused".into())),
        PatternAttributeType::string(),
    );
    for collection in [
        r#"secret(split("-", q.identifier))"#,
        r#"secret(secret(split("-", q.identifier)))"#,
        "private_values",
    ] {
        let parsed = crate::parser::parse(
            &format!(
                r#"
                    let q = mock.test.Queue {{ identifier = "a-b" }}
                    let private_values = secret(split("-", q.identifier))
                    mock.test.Consumer {{ value = {collection}[0] }}
                "#,
            ),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("secret-wrapped computed collection must parse");
        validate_resource_ref_types_for_test(&parsed, &schemas)
            .unwrap_or_else(|error| panic!("{collection}[0] must infer as String: {error}"));
    }
}

#[test]
fn resource_reference_segments_reject_wrong_shape_like_computed_access() {
    let parsed = crate::parser::parse(
        r#"
            let q = mock.test.Queue { identifier = "a-b" }
            mock.test.Consumer {
                value = (q.identifier)[0]
            }
            mock.test.Consumer {
                value = [q.identifier][0][0]
            }
        "#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("wrong-shape reference paths remain parseable for schema validation");
    let (_, schemas) = postfix_access_validation_fixture(
        Value::Concrete(ConcreteValue::String("unused".into())),
        PatternAttributeType::string(),
    );

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("indexing a String resource attribute must fail validation");
    assert_eq!(
        error.matches("cannot apply access [0] to String").count(),
        2,
        "both reference-rooted spellings must use the Access typecheck: {error}",
    );
}

#[test]
fn unknown_attribute_reference_suggests_similar_name() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.internet_gateway",
            vec![("internet_gateway_id", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.route",
            vec![
                ("route_table_id", PatternAttributeType::string()),
                ("gateway_id", PatternAttributeType::string()),
            ],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let igw = awscc.ec2.internet_gateway {}\nawscc.ec2.route { name = 'main-route' gateway_id = igw.internet_gateway_idd }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let result = validate_resource_ref_types_for_test(&parsed, &schemas);
    let err = result.unwrap_err();
    assert!(
        err.contains("Did you mean 'internet_gateway_id'?"),
        "Expected 'did you mean' suggestion, got: {}",
        err
    );
}

#[test]
fn unknown_attribute_reference_through_wait_binding_reports_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![
                ("certificate_arn", PatternAttributeType::string()),
                ("status", PatternAttributeType::string()),
            ],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (make_schema(
            "elasticloadbalancingv2.Listener",
            vec![("certificate_arn", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let cert = aws.acm.Certificate { status = 'pending' }\nlet cert_issued = wait cert { until = cert.status == 'issued' }\nlet listener = awscc.elasticloadbalancingv2.Listener { certificate_arn = cert_issued.certificate_arnn }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let err = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(
        err.contains("unknown attribute 'certificate_arnn' on 'cert_issued'"),
        "expected wait binding attribute error, got: {err}",
    );
    assert!(
        err.contains("reference cert_issued.certificate_arnn"),
        "expected wait binding reference in error, got: {err}",
    );
    assert!(
        err.contains("Did you mean 'certificate_arn'?"),
        "expected target schema suggestion, got: {err}",
    );
}

#[test]
fn known_attribute_reference_through_wait_binding_is_accepted() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![
                ("certificate_arn", PatternAttributeType::string()),
                ("status", PatternAttributeType::string()),
            ],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (make_schema(
            "elasticloadbalancingv2.Listener",
            vec![("certificate_arn", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let cert = aws.acm.Certificate { status = 'pending' }\nlet cert_issued = wait cert { until = cert.status == 'issued' }\nlet listener = awscc.elasticloadbalancingv2.Listener { certificate_arn = cert_issued.certificate_arn }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    assert!(validate_resource_ref_types_for_test(&parsed, &schemas).is_ok());
}

#[test]
fn resource_ref_string_is_assignable_to_ref_membered_sink_union() {
    let source_schema = make_schema("r.source", vec![("value", PatternAttributeType::string())]);
    let sink_schema = make_schema(
        "r.sink",
        vec![(
            "target",
            PatternAttributeType::union(vec![
                PatternAttributeType::ref_("S".to_string()),
                PatternAttributeType::ref_("L".to_string()),
            ]),
        )],
    )
    .with_def("S", PatternAttributeType::string())
    .with_def(
        "L",
        PatternAttributeType::list(PatternAttributeType::string()),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink_schema
            .validate()
            .expect("schema fixture must be valid"),
    );

    // Directory-shaped input: two resources and a let-binding reference,
    // matching the validation walk used for merged sibling files.
    let parsed = crate::parser::parse(
        r#"
let source = test.r.source {}

let sink = test.r.sink {
    target = source.value
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let result = validate_resource_ref_types_for_test(&parsed, &schemas);
    assert!(
        result.is_ok(),
        "String should flow into Union<Ref(S), Ref(L)> without a spurious error: {result:?}",
    );
}

#[test]
fn resource_ref_mismatch_displays_resolved_ref_type_names() {
    let source_schema = make_schema(
        "r.source",
        vec![("value", PatternAttributeType::ref_("Source".to_string()))],
    )
    .with_def("Source", PatternAttributeType::string());
    let sink_schema = make_schema(
        "r.sink",
        vec![("target", PatternAttributeType::ref_("Target".to_string()))],
    )
    .with_def("Target", PatternAttributeType::bool());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let parsed = crate::parser::parse(
        r#"
let source = test.r.source {}

let sink = test.r.sink {
    target = source.value
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("String must not be assignable to Bool");
    assert!(
        error.contains("expected Bool, got String"),
        "resolved source and sink names should be displayed: {error}",
    );
    assert!(
        !error.contains("Ref("),
        "schema-internal Ref names must not leak into the error: {error}",
    );
}

#[test]
fn resource_ref_mismatch_displays_resolved_union_member_names() {
    let source_schema = make_schema("r.source", vec![("value", PatternAttributeType::bool())]);
    let sink_schema = make_schema(
        "r.sink",
        vec![(
            "target",
            PatternAttributeType::union(vec![
                PatternAttributeType::ref_("S".to_string()),
                PatternAttributeType::ref_("L".to_string()),
            ]),
        )],
    )
    .with_def("S", PatternAttributeType::string())
    .with_def(
        "L",
        PatternAttributeType::list(PatternAttributeType::string()),
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "test",
        sink_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let parsed = crate::parser::parse(
        r#"
let source = test.r.source {}

let sink = test.r.sink {
    target = source.value
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let error = validate_resource_ref_types_for_test(&parsed, &schemas)
        .expect_err("Bool must not be assignable to String | List<String>");
    assert!(
        error.contains("expected String | List<String>, got Bool"),
        "nested Ref members should be resolved for display: {error}",
    );
    assert!(
        !error.contains("Ref("),
        "schema-internal Ref names must not leak into the error: {error}",
    );
}

#[test]
fn unknown_attribute_reference_through_wait_binding_inside_nested_struct_reports_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![
                ("certificate_arn", PatternAttributeType::string()),
                ("status", PatternAttributeType::string()),
            ],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (make_schema(
            "elasticloadbalancingv2.Listener",
            vec![(
                "certificates",
                PatternAttributeType::list(PatternAttributeType::struct_(
                    "Certificate".to_string(),
                    vec![
                        PatternStructField::optional(
                            "certificate_arn",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    ],
                )),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let cert = aws.acm.Certificate { status = 'pending' }\nlet cert_issued = wait cert { until = cert.status == 'issued' }\nlet listener = awscc.elasticloadbalancingv2.Listener { certificates = [{ certificate_arn = cert_issued.arn }] }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let err = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(
        err.contains("unknown attribute 'arn' on 'cert_issued'"),
        "expected nested wait binding attribute error, got: {err}",
    );
}

#[test]
fn unknown_attribute_reference_through_managed_binding_inside_nested_struct_reports_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![
                ("certificate_arn", PatternAttributeType::string()),
                ("status", PatternAttributeType::string()),
            ],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (make_schema(
            "elasticloadbalancingv2.Listener",
            vec![(
                "certificates",
                PatternAttributeType::list(PatternAttributeType::struct_(
                    "Certificate".to_string(),
                    vec![
                        PatternStructField::optional(
                            "certificate_arn",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    ],
                )),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let cert = aws.acm.Certificate { status = 'pending' }\nlet listener = awscc.elasticloadbalancingv2.Listener { certificates = [{ certificate_arn = cert.arn }] }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let err = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(
        err.contains("unknown attribute 'arn' on 'cert'"),
        "expected nested managed binding attribute error, got: {err}",
    );
}

#[test]
fn unknown_outer_attr_does_not_mask_nested_unknown_binding_ref() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "elasticloadbalancingv2.Listener",
            vec![("certificates", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let listener = awscc.elasticloadbalancingv2.Listener { certificates_typo = { certificate_arn = unknown_binding.foo } }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let err = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(
        err.contains("unknown binding 'unknown_binding' in reference unknown_binding.foo"),
        "expected nested unknown binding under unknown outer attr, got: {err}",
    );
}

#[test]
fn unknown_attribute_reference_no_suggestion_when_too_different() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.Vpc",
            vec![("cidr_block", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.Subnet",
            vec![("vpc_id", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");

    // Completely unrelated attribute name - no suggestion expected
    let subnet = Resource::with_provider("awscc", "ec2.Subnet", "web-subnet", None).with_attribute(
        "vpc_id",
        Value::resource_ref(
            "vpc".to_string(),
            "completely_wrong_name".to_string(),
            vec![],
        ),
    );

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    parsed.resources.push(subnet); // allow: direct — fixture test inspection
    let result = validate_resource_ref_types_for_test(&parsed, &schemas);
    let err = result.unwrap_err();
    assert!(
        !err.contains("Did you mean"),
        "Should not suggest when name is too different, got: {}",
        err
    );
}

#[test]
fn ref_type_mismatch_inside_for_body_is_rejected() {
    // Inside a for body, assigning an Int-typed attribute to a Bool-typed
    // target must be flagged.
    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        let vpc = test.r.vpc { name = "v" }
        for _, id in orgs.xs {
            test.r.pool_user {
                pool_id = vpc.vpc_id
            }
        }
    "#;
    let parsed = crate::parser::parse(
        src,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let mut schemas = SchemaRegistry::new();
    // test.r.vpc exposes `vpc_id: Int`
    schemas.insert(
        "test",
        (make_schema("r.vpc", vec![("vpc_id", PatternAttributeType::int())]))
            .validate()
            .expect("schema fixture must be valid"),
    );
    // test.r.pool_user requires `pool_id: Bool` — incompatible with Int.
    schemas.insert(
        "test",
        (make_schema(
            "r.pool_user",
            vec![("pool_id", PatternAttributeType::bool())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let result = validate_resource_ref_types_for_test(&parsed, &schemas);
    assert!(result.is_err(), "expected type-mismatch error in for body");
    let msg = result.unwrap_err();
    assert!(
        msg.contains("pool_id"),
        "expected error to mention pool_id, got: {msg}"
    );
}

#[test]
fn provider_in_module_with_arguments_errors() {
    let mut parsed = empty_parsed();
    parsed.providers.push(crate::parser::ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });
    parsed.arguments.push(crate::parser::ArgumentParameter {
        name: "vpc_cidr".to_string(),
        static_type: local_type(TypeExpr::String),
        default: None,
        description: None,
        validations: Vec::new(),
    });

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err(),
        "provider blocks are not allowed inside modules. Define providers at the root configuration level."
    );
}

#[test]
fn provider_in_module_with_attributes_errors() {
    let mut parsed = empty_parsed();
    parsed.providers.push(crate::parser::ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter::dummy(
            "vpc_id".to_string(),
            Some(TypeExpr::String),
            Some(Value::Concrete(ConcreteValue::String("dummy".into()))),
        ));

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_err());
}

#[test]
fn provider_without_module_markers_ok() {
    let mut parsed = empty_parsed();
    parsed.providers.push(crate::parser::ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_ok());
}

#[test]
fn module_without_provider_ok() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(crate::parser::ArgumentParameter {
        name: "vpc_cidr".to_string(),
        static_type: local_type(TypeExpr::String),
        default: None,
        description: None,
        validations: Vec::new(),
    });

    let result = validate_no_provider_in_module(&parsed);
    assert!(result.is_ok());
}

fn parse_state_blocks(source: &str) -> ParsedFile {
    crate::parser::parse(
        source,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("valid state block fixture")
}

#[test]
fn moved_in_module_with_arguments_errors() {
    let mut parsed = parse_state_blocks(
        r#"moved {
  from = mock.test.Resource 'old'
  to = mock.test.Resource 'new'
}
"#,
    );
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_state_blocks_in_module(&parsed);

    assert_eq!(
        result.unwrap_err(),
        "state blocks (moved, removed, and import) are not allowed inside modules. Define state blocks at the root configuration level."
    );
}

#[test]
fn removed_in_module_with_arguments_errors() {
    let mut parsed = parse_state_blocks(
        r#"removed {
  from = mock.test.Resource 'legacy'
}
"#,
    );
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_state_blocks_in_module(&parsed);

    assert!(result.is_err());
}

#[test]
fn import_in_module_with_arguments_errors() {
    let mut parsed = parse_state_blocks(
        r#"import {
  to = mock.test.Resource 'existing'
  id = 'resource-123'
}
"#,
    );
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_state_blocks_in_module(&parsed);

    assert!(result.is_err());
}

#[test]
fn state_block_in_module_with_attributes_errors() {
    let mut parsed = parse_state_blocks(
        r#"moved {
  from = mock.test.Resource 'old'
  to = mock.test.Resource 'new'
}
"#,
    );
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter::dummy(
            "resource_id".to_string(),
            Some(TypeExpr::String),
            Some(Value::Concrete(ConcreteValue::String("dummy".into()))),
        ));

    let result = validate_no_state_blocks_in_module(&parsed);

    assert!(result.is_err());
}

#[test]
fn state_blocks_without_module_markers_ok() {
    let parsed = parse_state_blocks(
        r#"moved {
  from = mock.test.Resource 'old'
  to = mock.test.Resource 'new'
}

removed {
  from = mock.test.Resource 'legacy'
}

import {
  to = mock.test.Resource 'existing'
  id = 'resource-123'
}
"#,
    );

    let result = validate_no_state_blocks_in_module(&parsed);

    assert!(result.is_ok());
}

#[test]
fn module_without_state_blocks_ok() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_state_blocks_in_module(&parsed);

    assert!(result.is_ok());
}

#[test]
fn backend_in_module_with_arguments_errors() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("name"));
    parsed.backend = Some(crate::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes: HashMap::new(),
        attribute_spans: HashMap::new(),
    });

    let result = validate_no_backend_in_module(&parsed);

    assert_eq!(
        result.unwrap_err(),
        "backend blocks are not allowed inside modules. Define the backend at the root configuration level."
    );
}

#[test]
fn backend_in_module_with_attributes_errors() {
    let mut parsed = empty_parsed();
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter::dummy(
            "resource_id".to_string(),
            Some(TypeExpr::String),
            Some(Value::Concrete(ConcreteValue::String("dummy".into()))),
        ));
    parsed.backend = Some(crate::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes: HashMap::new(),
        attribute_spans: HashMap::new(),
    });

    let result = validate_no_backend_in_module(&parsed);

    assert!(result.is_err());
}

#[test]
fn backend_without_module_markers_ok() {
    let mut parsed = empty_parsed();
    parsed.backend = Some(crate::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes: HashMap::new(),
        attribute_spans: HashMap::new(),
    });

    let result = validate_no_backend_in_module(&parsed);

    assert!(result.is_ok());
}

#[test]
fn module_without_backend_ok() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_backend_in_module(&parsed);

    assert!(result.is_ok());
}

fn parse_root_owned_declaration(source: &str) -> ParsedFile {
    crate::parser::parse(
        source,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("valid root-owned declaration fixture")
}

#[test]
fn upstream_state_in_module_with_arguments_errors() {
    let mut parsed =
        parse_root_owned_declaration("let up = upstream_state { source = '../other' }\n");
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_upstream_states_in_module(&parsed);

    assert_eq!(
        result.unwrap_err(),
        "upstream_state declarations are not allowed inside modules. Define upstream_state declarations at the root configuration level."
    );
}

#[test]
fn upstream_state_in_module_with_attributes_errors() {
    let mut parsed =
        parse_root_owned_declaration("let up = upstream_state { source = '../other' }\n");
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter::dummy(
            "resource_id".to_string(),
            Some(TypeExpr::String),
            Some(Value::Concrete(ConcreteValue::String("dummy".into()))),
        ));

    let result = validate_no_upstream_states_in_module(&parsed);

    assert!(result.is_err());
}

#[test]
fn upstream_state_without_module_markers_ok() {
    let parsed = parse_root_owned_declaration("let up = upstream_state { source = '../other' }\n");

    let result = validate_no_upstream_states_in_module(&parsed);

    assert!(result.is_ok());
}

#[test]
fn module_without_upstream_state_ok() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_upstream_states_in_module(&parsed);

    assert!(result.is_ok());
}

#[test]
fn exports_in_module_with_arguments_errors() {
    let mut parsed = parse_root_owned_declaration("exports {\n  module_value = 'module'\n}\n");
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_exports_in_module(&parsed);

    assert_eq!(
        result.unwrap_err(),
        "exports blocks are not allowed inside modules. Define exports at the root configuration level."
    );
}

#[test]
fn exports_in_module_with_attributes_errors() {
    let mut parsed = parse_root_owned_declaration("exports {\n  module_value = 'module'\n}\n");
    parsed
        .attribute_params
        .push(crate::parser::AttributeParameter::dummy(
            "resource_id".to_string(),
            Some(TypeExpr::String),
            Some(Value::Concrete(ConcreteValue::String("dummy".into()))),
        ));

    let result = validate_no_exports_in_module(&parsed);

    assert!(result.is_err());
}

#[test]
fn exports_without_module_markers_ok() {
    let parsed = parse_root_owned_declaration("exports {\n  root_value = 'root'\n}\n");

    let result = validate_no_exports_in_module(&parsed);

    assert!(result.is_ok());
}

#[test]
fn module_without_exports_ok() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("name"));

    let result = validate_no_exports_in_module(&parsed);

    assert!(result.is_ok());
}

// --- validate_no_arguments_in_root tests ---

fn argument_param(name: &str) -> crate::parser::ArgumentParameter {
    crate::parser::ArgumentParameter {
        name: name.to_string(),
        static_type: local_type(TypeExpr::String),
        default: None,
        description: None,
        validations: Vec::new(),
    }
}

fn provider(name: &str) -> crate::parser::ProviderConfig<ParserPlaceholder> {
    crate::parser::ProviderConfig {
        name: name.to_string(),
        attributes: IndexMap::new(),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    }
}

#[test]
fn arguments_with_backend_errors() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("state_path"));
    parsed.backend = Some(crate::parser::BackendConfig {
        backend_type: "local".to_string(),
        attributes: HashMap::new(),
        attribute_spans: HashMap::new(),
    });

    let result = validate_no_arguments_in_root(&parsed);
    assert_eq!(
        result.unwrap_err(),
        "arguments blocks are only valid inside module definitions, not in root configurations."
    );
}

#[test]
fn arguments_with_provider_errors() {
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("vpc_cidr"));
    parsed.providers.push(provider("aws"));

    let result = validate_no_arguments_in_root(&parsed);
    assert!(result.is_err());
}

#[test]
fn arguments_alone_is_ok() {
    // A directory with only `arguments` looks like a module the user
    // is validating in isolation; we cannot prove it is a root, so
    // do not flag it (issue #2198).
    let mut parsed = empty_parsed();
    parsed.arguments.push(argument_param("vpc_cidr"));

    let result = validate_no_arguments_in_root(&parsed);
    assert!(result.is_ok());
}

#[test]
fn empty_arguments_in_root_ok() {
    let parsed = empty_parsed();
    let result = validate_no_arguments_in_root(&parsed);
    assert!(result.is_ok());
}

// --- rendered_typecheck_result tests ---

#[test]
fn bare_builtin_custom_types_report_scalar_kind_mismatches() {
    for (name, value) in [
        ("positive_int", ConcreteValue::String("5".into())),
        ("positive_int", ConcreteValue::Float(crate::finite(5.0))),
        ("positive_int", ConcreteValue::Bool(true)),
        ("domain_name", ConcreteValue::Int(5)),
        ("email", ConcreteValue::Int(5)),
    ] {
        let expected = crate::parser::bare_type_for_test(name.to_string());
        let value = Value::Concrete(value);
        let error = typecheck::check_type_expr_value_with_context(
            &expected,
            &value,
            typecheck::TypecheckProviderContext::for_parser(&ProviderContext::new(
                crate::EnvironmentSnapshot::empty(),
            )),
            &crate::parser::retained::value_dependent_judgement_for_test(),
        )
        .require_provider_schemas()
        .expect_err("the declared scalar kind must be checked before its validator");
        assert!(
            matches!(
                &error,
                typecheck::TypeExprValueError::Mismatch {
                    validator_message: None,
                    ..
                }
            ),
            "{name}: {error}"
        );
        if name == "positive_int" && matches!(value, Value::Concrete(ConcreteValue::String(_))) {
            assert_eq!(error.to_string(), "expected PositiveInt, got string '5'");
        }
    }
}

#[test]
fn bare_builtin_custom_types_run_declared_validators() {
    for (name, invalid, message, valid) in [
        (
            "positive_int",
            ConcreteValue::Int(0),
            "Value must be positive",
            ConcreteValue::Int(5),
        ),
        (
            "domain_name",
            ConcreteValue::String("a_b.com".into()),
            "Invalid domain name 'a_b.com'",
            ConcreteValue::String("example.com".into()),
        ),
        (
            "email",
            ConcreteValue::String("invalid-email".into()),
            "Invalid email",
            ConcreteValue::String("user@example.com".into()),
        ),
    ] {
        let expected = crate::parser::bare_type_for_test(name.to_string());
        let error = rendered_typecheck_result(
            &expected,
            &Value::Concrete(invalid),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .expect("the declaration's validator must reject invalid values of the right kind");
        assert!(error.contains(message), "{name}: {error}");
        assert!(
            rendered_typecheck_result(
                &expected,
                &Value::Concrete(valid),
                &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
            )
            .is_none(),
            "{name}: valid value must pass"
        );
    }
}

#[test]
fn bare_builtin_custom_types_validate_enum_identifier_text() {
    for (name, text, valid) in [
        ("domain_name", "example.com", true),
        ("domain_name", "a_b.com", false),
        ("positive_int", "5", false),
    ] {
        let error = rendered_typecheck_result(
            &crate::parser::bare_type_for_test(name.to_string()),
            &Value::Concrete(ConcreteValue::enum_identifier(text.to_string())),
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        );
        assert_eq!(error.is_none(), valid, "{name}: {text}: {error:?}");
        if let Some(error) = error {
            assert!(!error.contains("unknown custom type"), "{error}");
        }
    }
}

#[test]
fn rendered_typecheck_result_error_uses_pascal_case() {
    let msg = rendered_typecheck_result(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("should error");
    assert!(msg.contains("expected String"), "got: {msg}");
}

#[test]
fn validate_type_expr_struct_error_uses_pascal_case_in_field_type() {
    let mut map = indexmap::IndexMap::new();
    map.insert(
        "count".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    let fields = vec![("count".to_string(), TypeExpr::Int)];
    let msg = rendered_typecheck_result(
        &TypeExpr::Struct { fields },
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("should error");
    assert!(
        msg.contains("field 'count'") && msg.contains("Int"),
        "got: {msg}"
    );
}

#[test]
fn rendered_typecheck_result_ipv4_cidr_valid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv4_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_none());
}

#[test]
fn rendered_typecheck_result_ipv4_cidr_invalid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv4_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("not-a-cidr".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
}

#[test]
fn rendered_typecheck_result_ipv4_address_valid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv4_address".to_string()),
        &Value::Concrete(ConcreteValue::String("192.168.1.1".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_none());
}

#[test]
fn rendered_typecheck_result_ipv4_address_invalid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv4_address".to_string()),
        &Value::Concrete(ConcreteValue::String("999.999.999.999".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
}

#[test]
fn rendered_typecheck_result_ipv6_cidr_valid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv6_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("2001:db8::/32".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_none());
}

#[test]
fn rendered_typecheck_result_ipv6_cidr_invalid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv6_cidr".to_string()),
        &Value::Concrete(ConcreteValue::String("not-ipv6-cidr".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
}

#[test]
fn rendered_typecheck_result_ipv6_address_valid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv6_address".to_string()),
        &Value::Concrete(ConcreteValue::String("2001:db8::1".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_none());
}

#[test]
fn rendered_typecheck_result_ipv6_address_invalid() {
    let result = rendered_typecheck_result(
        &crate::parser::bare_type_for_test("ipv6_address".to_string()),
        &Value::Concrete(ConcreteValue::String("zzz::zzz".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
}

#[test]
fn rendered_typecheck_result_bool_mismatch() {
    let result = rendered_typecheck_result(
        &TypeExpr::Bool,
        &Value::Concrete(ConcreteValue::String("yes".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool"));
}

#[test]
fn rendered_typecheck_result_int_mismatch() {
    let result = rendered_typecheck_result(
        &TypeExpr::Int,
        &Value::Concrete(ConcreteValue::String("42".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int"));
}

#[test]
fn rendered_typecheck_result_float_mismatch() {
    let result = rendered_typecheck_result(
        &TypeExpr::Float,
        &Value::Concrete(ConcreteValue::String("3.14".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Float"));
}

#[test]
fn rendered_typecheck_result_list_of_ipv4_address() {
    let items = vec![
        Value::Concrete(ConcreteValue::String("192.168.1.1".into())),
        Value::Concrete(ConcreteValue::String("999.0.0.1".into())),
    ];
    let result = rendered_typecheck_result(
        &TypeExpr::List(Box::new(crate::parser::bare_type_for_test(
            "ipv4_address".to_string(),
        ))),
        &Value::Concrete(ConcreteValue::List(items)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("list element 1"));
}

#[test]
fn rendered_typecheck_result_string_type_accepts_string() {
    let result = rendered_typecheck_result(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::String("hello".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_none());
}

#[test]
fn rendered_typecheck_result_string_got_bool() {
    let result = rendered_typecheck_result(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Bool(true)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got bool"));
}

#[test]
fn rendered_typecheck_result_string_got_int() {
    let result = rendered_typecheck_result(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got int"));
}

#[test]
fn rendered_typecheck_result_string_got_float() {
    let result = rendered_typecheck_result(
        &TypeExpr::String,
        &Value::Concrete(ConcreteValue::Float(crate::finite(1.5))),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected String, got float"));
}

#[test]
fn rendered_typecheck_result_bool_got_int() {
    let result = rendered_typecheck_result(
        &TypeExpr::Bool,
        &Value::Concrete(ConcreteValue::Int(1)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool, got int"));
}

#[test]
fn rendered_typecheck_result_int_got_bool() {
    let result = rendered_typecheck_result(
        &TypeExpr::Int,
        &Value::Concrete(ConcreteValue::Bool(true)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int, got bool"));
}

#[test]
fn rendered_typecheck_result_float_got_bool() {
    let result = rendered_typecheck_result(
        &TypeExpr::Float,
        &Value::Concrete(ConcreteValue::Bool(false)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Float, got bool"));
}

#[test]
fn rendered_typecheck_result_bool_got_float_is_rejected() {
    // Regression for #2864: Float must not silently coerce into Bool.
    let result = rendered_typecheck_result(
        &TypeExpr::Bool,
        &Value::Concrete(ConcreteValue::Float(crate::finite(1.5))),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Bool, got float"));
}

#[test]
fn rendered_typecheck_result_int_got_float_is_rejected() {
    // Regression for #2864: Float must not silently coerce into Int.
    let result = rendered_typecheck_result(
        &TypeExpr::Int,
        &Value::Concrete(ConcreteValue::Float(crate::finite(1.5))),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected Int, got float"));
}

#[test]
fn rendered_typecheck_result_escapes_string_literal_value() {
    let result = rendered_typecheck_result(
        &TypeExpr::Bool,
        &Value::Concrete(ConcreteValue::String("yes\"\nno".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    assert_eq!(
        result.as_deref(),
        Some("expected Bool, got string \"yes\\\"\\nno\". Use true or false.")
    );
}

#[test]
fn rendered_typecheck_result_preserves_float_fraction() {
    let result = rendered_typecheck_result(
        &TypeExpr::Int,
        &Value::Concrete(ConcreteValue::Float(crate::finite(1.0))),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    assert_eq!(result.as_deref(), Some("expected Int, got float (1.0)"));
}

#[test]
fn rendered_typecheck_result_struct_got_string_includes_literal() {
    let type_expr = TypeExpr::Struct {
        fields: vec![("environment".to_string(), TypeExpr::String)],
    };
    let result = rendered_typecheck_result(
        &type_expr,
        &Value::Concrete(ConcreteValue::String("production".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let message = result.expect("a string must not satisfy a struct type");
    assert!(
        message.contains("got string 'production'"),
        "struct mismatch should include the scalar literal; got: {message}"
    );
}

#[test]
fn rendered_typecheck_result_float_got_int_is_accepted() {
    // Intentional one-way widening: Int flows into Float (see #2864).
    let result = rendered_typecheck_result(
        &TypeExpr::Float,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_none());
}

#[test]
fn rendered_typecheck_result_schema_type_accepts_string() {
    let owned = crate::parser::OwnedTypeIdentity::provider(
        "awscc",
        TypeIdentity::from_schema_type("awscc", "ec2", "VpcId"),
        crate::schema::RefinedScalarKind::String,
    );
    let schema_type = TypeExpr::SchemaType {
        owned: owned.clone().unbounded_witness_for_test(),
    };
    let context = context_with_accepting_custom_type_resolver(HashSet::from([owned]));
    let result = rendered_typecheck_result(
        &schema_type,
        &Value::Concrete(ConcreteValue::String("vpc-12345678".into())),
        &context,
    );
    assert!(result.is_none());
}

#[test]
fn rendered_typecheck_result_schema_type_rejects_bool() {
    let schema_type = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "awscc",
            TypeIdentity::from_schema_type("awscc", "ec2", "VpcId"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let result = rendered_typecheck_result(
        &schema_type,
        &Value::Concrete(ConcreteValue::Bool(true)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected awscc.ec2.VpcId"));
}

#[test]
fn rendered_typecheck_result_schema_type_rejects_int() {
    let schema_type = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "awscc",
            TypeIdentity::from_schema_type("awscc", "ec2", "VpcId"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let result = rendered_typecheck_result(
        &schema_type,
        &Value::Concrete(ConcreteValue::Int(42)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("expected awscc.ec2.VpcId"));
}

#[test]
fn discard_binding_no_warning() {
    let mut parsed = empty_parsed();

    let caller = Resource::with_provider("aws", "sts.caller_identity", "caller_identity", None)
        .with_binding("_");
    parsed.resources.push(caller); // allow: direct — fixture test inspection

    assert!(check_unused_bindings_in_file(&parsed).is_empty());
}

#[test]
fn validate_type_expr_custom_type_rejects_invalid() {
    let config = context_with_iam_policy_arn_validator();

    let result = rendered_typecheck_result(
        &iam_policy_arn_type_expr(),
        &Value::Concrete(ConcreteValue::String("aaaa".into())),
        &config,
    );
    assert!(result.is_some(), "Expected validation error for 'aaaa'");
    assert!(result.unwrap().contains("invalid IAM policy ARN"));

    let result = rendered_typecheck_result(
        &iam_policy_arn_type_expr(),
        &Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::123456789012:policy/MyPolicy"
                .to_string()
                .into(),
        )),
        &config,
    );
    assert!(result.is_none(), "Expected no error for valid ARN");
}

#[test]
fn validate_type_expr_list_custom_type_rejects_invalid() {
    let config = context_with_iam_policy_arn_validator();

    let result = rendered_typecheck_result(
        &TypeExpr::List(Box::new(iam_policy_arn_type_expr())),
        &Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
            ConcreteValue::String("aaaa".into()),
        )])),
        &config,
    );
    assert!(
        result.is_some(),
        "Expected validation error for list element"
    );
    assert!(result.unwrap().contains("list element 0"));
}

fn struct_type_name_value() -> TypeExpr {
    TypeExpr::Struct {
        fields: vec![
            ("name".to_string(), TypeExpr::String),
            ("value".to_string(), TypeExpr::Int),
        ],
    }
}

#[test]
fn validate_type_expr_struct_accepts_well_formed_map() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    map.insert("value".to_string(), Value::Concrete(ConcreteValue::Int(1)));
    let result = rendered_typecheck_result(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_none(), "got error: {:?}", result);
}

#[test]
fn validate_type_expr_struct_rejects_missing_field() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    let result = rendered_typecheck_result(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert_eq!(
        result.as_deref(),
        Some("expected struct, missing field 'value'.")
    );
}

#[test]
fn validate_type_expr_struct_rejects_unknown_field() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    map.insert("value".to_string(), Value::Concrete(ConcreteValue::Int(1)));
    map.insert(
        "extra".to_string(),
        Value::Concrete(ConcreteValue::String("y".into())),
    );
    let result = rendered_typecheck_result(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert_eq!(
        result.as_deref(),
        Some("expected struct, unknown field 'extra'.")
    );
}

#[test]
fn validate_type_expr_struct_rejects_wrong_field_type() {
    let mut map = IndexMap::new();
    map.insert(
        "name".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    map.insert(
        "value".to_string(),
        Value::Concrete(ConcreteValue::String("not-an-int".into())),
    );
    let result = rendered_typecheck_result(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::Map(map)),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    let msg = result.unwrap();
    assert!(msg.contains("value"), "unexpected error: {msg}");
}

#[test]
fn validate_type_expr_struct_rejects_non_map_value() {
    let result = rendered_typecheck_result(
        &struct_type_name_value(),
        &Value::Concrete(ConcreteValue::String("oops".into())),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert!(result.is_some());
    assert!(result.unwrap().contains("struct"));
}

#[test]
fn rendered_struct_typecheck_result_is_deterministic_for_multiple_unknowns() {
    // Multiple unknown keys must produce the *same* alphabetically-first
    // error on every call, independent of HashMap's per-process random
    // hash seed.
    let fields: Vec<(String, TypeExpr)> = vec![("a".to_string(), TypeExpr::String)];
    let mut entries: IndexMap<String, Value> = IndexMap::new();
    entries.insert(
        "a".to_string(),
        Value::Concrete(ConcreteValue::String("ok".into())),
    );
    entries.insert(
        "z_extra".to_string(),
        Value::Concrete(ConcreteValue::String("x".into())),
    );
    entries.insert(
        "b_extra".to_string(),
        Value::Concrete(ConcreteValue::String("y".into())),
    );
    entries.insert(
        "m_extra".to_string(),
        Value::Concrete(ConcreteValue::String("z".into())),
    );

    let first = rendered_struct_typecheck_result(&fields, &entries);
    for _ in 0..20 {
        assert_eq!(first, rendered_struct_typecheck_result(&fields, &entries));
    }
    assert_eq!(
        first.as_deref(),
        Some("expected struct, unknown field 'b_extra'.")
    );
}

#[test]
fn lifted_struct_rejects_missing_schema_field_when_source_has_extra() {
    // Regression: the old `expr.iter().all(find in schema)` logic let an
    // expr struct omit a required schema field as long as sizes matched.
    // The bijection fix must reject this.
    use crate::schema::CompiledStructField as StructField;
    let expr = TypeExpr::Struct {
        fields: vec![
            ("a".to_string(), TypeExpr::Int),
            ("c".to_string(), TypeExpr::Int),
        ],
    };
    let schema = AttributeType::struct_(
        "Row".to_string(),
        vec![
            StructField::optional("a", AttributeType::int()).build(),
            StructField::optional("b", AttributeType::string()).build(),
        ],
    );
    assert!(!lifted_source_is_assignable_to_schema(
        &expr,
        &schema,
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn lifted_struct_matches_same_shape_schema() {
    use crate::schema::CompiledStructField as StructField;
    let expr = TypeExpr::Struct {
        fields: vec![
            ("name".to_string(), TypeExpr::String),
            ("value".to_string(), TypeExpr::Int),
        ],
    };
    let schema = AttributeType::struct_(
        "Row".to_string(),
        vec![
            StructField::optional("name", AttributeType::string()).build(),
            StructField::optional("value", AttributeType::int()).build(),
        ],
    );
    assert!(lifted_source_is_assignable_to_schema(
        &expr,
        &schema,
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn lifted_struct_flows_into_map_when_fields_share_type() {
    // A downstream consumer annotated `map(string)` accepts a
    // `struct { a: string, b: string }` — every field satisfies string.
    let expr = TypeExpr::Struct {
        fields: vec![
            ("a".to_string(), TypeExpr::String),
            ("b".to_string(), TypeExpr::String),
        ],
    };
    let schema = AttributeType::map_with_key(AttributeType::string(), AttributeType::string());
    assert!(lifted_source_is_assignable_to_schema(
        &expr,
        &schema,
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn lifted_struct_rejects_map_with_wrong_element_type() {
    let expr = TypeExpr::Struct {
        fields: vec![("a".to_string(), TypeExpr::String)],
    };
    let schema = AttributeType::map_with_key(AttributeType::string(), AttributeType::int());
    assert!(!lifted_source_is_assignable_to_schema(
        &expr,
        &schema,
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn directional_relation_rejects_plain_string_for_identified_sink() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let schema = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("VpcId"),
        None,
        None,
        legacy_validator(move |value| noop(value).map_err(Into::into)),
        None,
    );
    assert!(
        !lifted_source_is_assignable_to_schema(
            &TypeExpr::String,
            &schema,
            crate::schema::empty_defs_for_schema_walks()
        ),
        "TypeExpr::String must not satisfy Custom{{semantic_name:VpcId}}"
    );
}

// An unrefined source supplies no refinement evidence to overlap-or-defer (#4362).
// The identity-less sink's pattern is checked on the resolved destination value.
#[test]
fn directional_relation_accepts_plain_string_for_identityless_pattern_sink() {
    let schema = AttributeType::refined_string(
        None,
        Some(
            crate::schema::CompiledPattern::try_new(&crate::schema::PatternSpec::new(
                "^.+$",
                crate::schema::PatternMatch::Full,
            ))
            .unwrap(),
        ),
        None,
        None,
    );
    assert!(
        lifted_source_is_assignable_to_schema(
            &TypeExpr::String,
            &schema,
            crate::schema::empty_defs_for_schema_walks()
        ),
        "plain String should defer an identity-less pattern to value validation"
    );
}

// Under the shared directional relation a union sink accepts a source when
// any member accepts it. This intentionally differs from the removed ad-hoc
// #2358 union rule; the direct String -> VpcId rejection remains pinned above.
#[test]
fn directional_relation_accepts_string_through_plain_union_member() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let schema = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare("VpcId"),
            None,
            None,
            legacy_validator(move |value| noop(value).map_err(Into::into)),
            None,
        ),
    ]);
    assert!(
        lifted_source_is_assignable_to_schema(
            &TypeExpr::String,
            &schema,
            crate::schema::empty_defs_for_schema_walks()
        ),
        "the union's plain String member accepts the source"
    );
}

// Companion: a Union of *only* specific Custom alternatives is even
// more clearly String-incompatible — every branch demands a specific
// identity that String can't prove.
#[test]
fn directional_relation_rejects_string_for_union_of_only_identified_sinks() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let mk = |name: &str| {
        AttributeType::refined_string_with_validator(
            TypeIdentity::bare(name),
            None,
            None,
            legacy_validator(move |value| noop(value).map_err(Into::into)),
            None,
        )
    };
    let schema = AttributeType::union(vec![mk("VpcId"), mk("SubnetId")]);
    assert!(
        !lifted_source_is_assignable_to_schema(
            &TypeExpr::String,
            &schema,
            crate::schema::empty_defs_for_schema_walks()
        ),
        "TypeExpr::String must not satisfy a Union of only specific-Custom alternatives"
    );
}

// Companion: a Union of only string-shaped, non-specific types must
// keep accepting `TypeExpr::String` (no specific-Custom alternative
// exists, so the value can safely flow into any branch).
#[test]
fn directional_relation_accepts_string_for_union_with_plain_string_member() {
    let schema = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::enum_(
            TypeIdentity::bare("Mode"),
            Some(vec!["A".to_string(), "B".to_string()]),
            vec![],
            None,
            None,
        ),
    ]);
    assert!(
        lifted_source_is_assignable_to_schema(
            &TypeExpr::String,
            &schema,
            crate::schema::empty_defs_for_schema_walks()
        ),
        "TypeExpr::String must satisfy a Union of only string-shaped non-specific types"
    );
}

// Companion: the safe direction — a specific Custom-typed export must
// continue to satisfy a receiver of the same Custom type. Pin so the
// strictness fix doesn't accidentally reject the correct direction.
#[test]
fn lifted_simple_vpc_id_accepts_matching_custom_identity() {
    use crate::schema::legacy_validator;
    fn noop(_v: &crate::resource::Value) -> Result<(), String> {
        Ok(())
    }
    let schema = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("VpcId"),
        None,
        None,
        legacy_validator(move |value| noop(value).map_err(Into::into)),
        None,
    );
    // Parser normalizes `: VpcId` to crate::parser::bare_type_for_test("vpc_id") (snake).
    let expr = crate::parser::bare_type_for_test("vpc_id".to_string());
    assert!(lifted_source_is_assignable_to_schema(
        &expr,
        &schema,
        crate::schema::empty_defs_for_schema_walks()
    ));
}

#[test]
fn check_module_calls_providerless_defers_unloaded_bare_count_argument() {
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let module = crate::parser::parse("arguments { count: Count }", &config).unwrap();
    let parsed =
        crate::parser::parse("let m = use { source = './m' }\nm { count = 3 }", &config).unwrap();
    let signatures = HashMap::from([(
        "m".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: module
                .arguments
                .into_iter()
                .map(|argument| crate::parser::ArgumentParameter {
                    name: argument.name,
                    static_type: consumer_type(argument.static_type),
                    default: argument.default,
                    description: argument.description,
                    validations: argument.validations,
                })
                .collect(),
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    let schemas = SchemaRegistry::new();
    let report = check_bare_binding_uses(&ValidationTypeContext::new_providerless(
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config).judged(),
        &config,
        &signatures,
    ));
    let bindings =
        BindingIndex::from_parsed_with_module_signatures(&parsed, &schemas, &signatures, &config);
    let findings = crate::validation::deferred_types::with_session_for_test(&config, |session| {
        check_module_calls_providerless(ModuleCallCheckInput {
            authored: &parsed,
            semantic: Some(&parsed),
            base_dir: None,
            imported_signatures: &signatures,
            provider_context: session,
            schemas: &schemas,
            binding_use_report: &report,
            bindings: &bindings,
            reference_checks: ModuleCallReferenceChecks::Enabled,
        })
        .unwrap()
    });
    assert!(findings.is_empty(), "{findings:#?}");
}

#[test]
fn check_module_calls_rejects_custom_type() {
    use crate::parser::ArgumentParameter;

    let config = context_with_iam_policy_arn_validator();

    let parsed = crate::parser::parse_and_resolve(
        "let github = use { source = './github' }\ngithub { managed_policy_arns = ['aaaa'] }",
    )
    .unwrap();

    let mut imported_modules = HashMap::new();
    imported_modules.insert(
        "github".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "managed_policy_arns".to_string(),
                static_type: consumer_type(TypeExpr::List(Box::new(iam_policy_arn_type_expr()))),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    );

    let findings = module_call_findings_for_test(&parsed, &imported_modules, &config);
    assert!(
        findings.iter().any(|finding| finding
            .message()
            .to_string()
            .contains("invalid IAM policy ARN")),
        "unexpected findings: {findings:#?}"
    );
}

#[test]
fn check_module_calls_rejects_literal_union_mismatch() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse_and_resolve(
        "let web_tier = use { source = './web' }\nweb_tier { environment = 'production' }",
    )
    .unwrap();
    let imported_modules = HashMap::from([(
        "web_tier".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "environment".to_string(),
                static_type: consumer_type(TypeExpr::Union(vec![
                    TypeExpr::StringLiteral("dev".to_string()),
                    TypeExpr::StringLiteral("stg".to_string()),
                    TypeExpr::StringLiteral("prod".to_string()),
                ])),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let findings = module_call_findings_for_test(
        &parsed,
        &imported_modules,
        &LoadedProviderContext::empty_for_test(),
    );

    assert!(
        findings.iter().any(|finding| finding
            .message()
            .to_string()
            .contains("expected 'dev' | 'stg' | 'prod', got string 'production'")),
        "unexpected findings: {findings:#?}"
    );
}

#[test]
fn check_module_calls_reports_parameter_type_resolution_errors() {
    use crate::parser::ArgumentParameter;

    let mut parsed = empty_parsed();
    parsed.module_calls = vec![ModuleCall::test(
        "child",
        Some("instance".to_string()),
        module_arguments([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("input".into())),
        )]),
    )];
    let signatures = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "value".to_string(),
                static_type: consumer_type(TypeExpr::DottedUnresolved(
                    crate::parser::ResourceTypePath::new("aws", "missing.Type"),
                )),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    let provider_context = context_with_known_custom_types(HashSet::new());

    let findings = module_call_findings_for_test(&parsed, &signatures, &provider_context);

    assert_eq!(findings.len(), 1, "unexpected findings: {findings:#?}");
    assert!(
        findings[0]
            .message()
            .to_string()
            .contains("unknown custom type 'aws.missing.Type'"),
        "type-resolution error must remain a module-call finding: {findings:#?}",
    );
}

#[test]
fn check_module_calls_reports_unnameable_signature_type_resolution_errors() {
    use crate::parser::ArgumentParameter;

    let mut parsed = empty_parsed();
    parsed.module_calls = vec![ModuleCall::test(
        "child",
        Some("instance".to_string()),
        module_arguments([(
            "value".to_string(),
            Value::Concrete(ConcreteValue::String("input".into())),
        )]),
    )];
    let unresolved =
        TypeExpr::DottedUnresolved(crate::parser::ResourceTypePath::new("aws", "missing.Type"));
    let provider_context = context_with_known_custom_types(HashSet::new());
    let static_type = resolve_consumer_static_type_for_signature(&unresolved, &provider_context);
    assert!(
        matches!(
            &static_type,
            crate::static_type::StaticType::Unchecked(reason)
                if reason.original() == &unresolved
                    && matches!(
                        reason.resolution_error(),
                        crate::static_type::StaticTypeResolutionError::UnknownCustomType { .. }
                    )
        ),
        "an unresolved consumer signature must be typed as unchecked: {static_type:?}",
    );
    let signatures = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "value".to_string(),
                static_type,
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let findings = module_call_findings_for_test(&parsed, &signatures, &provider_context);

    assert_eq!(findings.len(), 1, "unexpected findings: {findings:#?}");
    assert!(
        findings[0]
            .message()
            .to_string()
            .contains("unknown custom type 'aws.missing.Type'"),
        "the typed unchecked reason must remain a module-call finding: {findings:#?}",
    );
}

#[test]
fn check_module_calls_reports_nested_file_type_resolution_errors() {
    let fixture = tempfile::tempdir().expect("nested type-resolution fixture");
    let child = fixture.path().join("child");
    std::fs::create_dir_all(&child).expect("create child module");
    std::fs::write(
        child.join("main.crn"),
        "attributes { bad: aws.missing.Type = \"value\" }\n",
    )
    .expect("write child module");
    let root_path = fixture.path().join("main.crn");
    let parsed = crate::parser::parse_file(
        "let child = use { source = \"./child\" }\nchild {}\n",
        &root_path,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("root fixture must parse");
    let provider_context = context_with_known_custom_types(HashSet::new());
    let schemas = SchemaRegistry::new();
    let signatures = crate::module_resolver::load_resolved_module_signatures(
        &parsed,
        &crate::paths::ResolvedPath::new(fixture.path()).unwrap(),
        &provider_context,
    );
    let binding_use_report = check_bare_binding_uses(&ValidationTypeContext::new(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            &schemas,
            provider_context.as_provider_context(),
        )
        .judged(),
        &provider_context,
        &signatures,
    ));
    let bindings = BindingIndex::from_parsed_with_module_signatures(
        &parsed,
        &schemas,
        &signatures,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let findings =
        crate::validation::deferred_types::with_session_for_test(&provider_context, |session| {
            check_module_calls(ModuleCallCheckInput {
                authored: &parsed,
                semantic: Some(&parsed),
                base_dir: Some(&crate::paths::ResolvedPath::new(fixture.path()).unwrap()),
                imported_signatures: &signatures,
                provider_context: session,
                schemas: &schemas,
                binding_use_report: &binding_use_report,
                bindings: &bindings,
                reference_checks: ModuleCallReferenceChecks::Enabled,
            })
            .unwrap()
        });

    assert_eq!(findings.len(), 1, "unexpected findings: {findings:#?}");
    assert!(
        findings[0]
            .message()
            .to_string()
            .contains("attribute 'bad': unknown custom type 'aws.missing.Type'"),
        "nested declaration resolution errors must not be discarded: {findings:#?}",
    );
}

#[test]
fn check_module_calls_uses_original_line_after_heredoc() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse(
        r#"let m = use { source = './m' }
let document = <<EOT
alpha
beta
gamma
delta
EOT
let inst = m { n = 'abc' }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let imported_modules = HashMap::from([(
        "m".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "n".to_string(),
                static_type: consumer_type(TypeExpr::Int),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let findings = module_call_findings_for_test(
        &parsed,
        &imported_modules,
        &LoadedProviderContext::empty_for_test(),
    );
    let finding = findings
        .first()
        .expect("the string argument must be rejected");

    assert_eq!(finding.anchor().span.start_line(), 8);
}

#[test]
fn check_module_calls_uses_argument_values_and_call_span_as_authored_anchors() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse(
        r#"let child = use { source = './child' }
child {
  surprise = true
  count = "bad"
}
child {}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let signatures = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "count".to_string(),
                static_type: consumer_type(TypeExpr::Int),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let findings = module_call_findings_for_test(
        &parsed,
        &signatures,
        &LoadedProviderContext::empty_for_test(),
    );
    let first_call = &parsed.module_calls[0];
    let second_call = &parsed.module_calls[1];
    let surprise_span = first_call
        .arguments_with_source_spans()
        .find(|argument| argument.name() == "surprise")
        .expect("surprise argument")
        .key_span();
    let count_span = first_call
        .arguments_with_source_spans()
        .find(|argument| argument.name() == "count")
        .expect("count argument")
        .value_span();

    let anchor_for = |kind| {
        findings
            .iter()
            .find(|finding| finding.kind() == kind)
            .unwrap_or_else(|| panic!("missing {kind:?}: {findings:#?}"))
            .anchor()
    };
    assert_eq!(
        anchor_for(ModuleCallFindingKind::UnknownArgument).span,
        surprise_span,
    );
    assert_eq!(
        anchor_for(ModuleCallFindingKind::ValueType).span,
        count_span,
    );
    assert_eq!(
        anchor_for(ModuleCallFindingKind::MissingRequiredArgument).span,
        second_call.source_span(),
    );
    assert!(
        findings
            .iter()
            .all(|finding| { finding.anchor().source_file.path() == first_call.source_file() })
    );
}

#[test]
fn check_module_calls_keeps_distinct_for_expansion_payloads() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse(
        r#"let child = use { source = "./child" }
for item in ["first", "second"] {
  child { count = item }
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    assert_eq!(parsed.module_calls.len(), 2);
    let signatures = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "count".to_string(),
                static_type: consumer_type(TypeExpr::Int),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let findings = module_call_findings_for_test(
        &parsed,
        &signatures,
        &LoadedProviderContext::empty_for_test(),
    );
    let mismatches = findings
        .iter()
        .filter(|finding| finding.kind() == ModuleCallFindingKind::ValueType)
        .collect::<Vec<_>>();

    assert_eq!(mismatches.len(), 2, "distinct payloads: {findings:#?}");
    assert_eq!(mismatches[0].anchor(), mismatches[1].anchor());
    let messages = mismatches
        .iter()
        .map(|finding| finding.message().to_string())
        .collect::<Vec<_>>();
    assert!(messages.iter().any(|message| message.contains("'first'")));
    assert!(messages.iter().any(|message| message.contains("'second'")));
}

#[test]
fn check_module_calls_pending_binding_slot_owns_missing_argument_suppression() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse(
        r#"let vpc = mock.test.Thing { name = "source" }
let child = use { source = "./child" }
child { values = [vpc] }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let signatures = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "values".to_string(),
                static_type: consumer_type(TypeExpr::List(Box::new(TypeExpr::String))),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let findings = module_call_findings_for_test(
        &parsed,
        &signatures,
        &LoadedProviderContext::empty_for_test(),
    );

    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.kind() == ModuleCallFindingKind::BindingUse)
            .count(),
        1,
        "the pending handle must retain its authored binding finding: {findings:#?}",
    );
    assert!(
        findings
            .iter()
            .all(|finding| { finding.kind() != ModuleCallFindingKind::MissingRequiredArgument }),
        "the pending slot must not also become a missing argument: {findings:#?}",
    );
}

#[test]
fn expanded_module_argument_binding_findings_block_recovery_pipeline() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse(
        r#"let vpc = mock.test.Thing { name = "source" }
let child = use { source = "./child" }
child { values = [vpc] }
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let signatures = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "values".to_string(),
                static_type: consumer_type(TypeExpr::List(Box::new(TypeExpr::String))),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    let schemas = SchemaRegistry::new();
    let report = check_bare_binding_uses(&ValidationTypeContext::new_providerless(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .judged(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &signatures,
    ));
    assert!(!report.is_ready(), "fixture must produce a binding finding");

    let findings = check_expanded_module_calls::<crate::resource::ParserPlaceholder>(
        ExpandedModuleCallCheckInput::BindingUses {
            binding_use_report: &report,
            compositions: &[],
        },
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    assert_eq!(findings.len(), 1, "unexpected findings: {findings:#?}");
    assert_eq!(findings[0].kind(), ModuleCallFindingKind::BindingUse);
}

#[test]
fn authored_and_expanded_binding_findings_merge_by_typed_identity() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse_file(
        r#"let source = mock.counter { count = 7 }
let forwarded = source.count
let child_module = use { source = "./child" }
let child = child_module { value = forwarded }
"#,
        std::path::Path::new("calls.crn"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("binding-reference fixture must parse");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("counter")
            .attribute(
                PatternAttributeSchema::optional("count", PatternAttributeType::int()).build(),
            )
            .validate()
            .expect("schema fixture must be valid"),
    );
    let signatures = HashMap::from([(
        "child_module".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "value".to_string(),
                static_type: consumer_type(TypeExpr::String),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    let report = check_bare_binding_uses(&ValidationTypeContext::new_providerless(
        crate::config_loader::load_buffer_configuration(
            &parsed,
            &schemas,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        )
        .judged(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &signatures,
    ));
    assert_eq!(report.findings().len(), 1, "unexpected report: {report:#?}");
    assert!(matches!(
        report.findings()[0].error,
        BindingResolutionError::ReferenceType(_)
    ));
    let bindings = BindingIndex::from_parsed_with_module_signatures(
        &parsed,
        &schemas,
        &signatures,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let authored = crate::validation::deferred_types::with_session_for_test(
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        |session| {
            check_module_calls_providerless(ModuleCallCheckInput {
                authored: &parsed,
                semantic: Some(&parsed),
                base_dir: None,
                imported_signatures: &signatures,
                provider_context: session,
                schemas: &schemas,
                binding_use_report: &report,
                bindings: &bindings,
                reference_checks: ModuleCallReferenceChecks::Enabled,
            })
            .unwrap()
        },
    );
    let expanded = check_expanded_module_calls::<crate::resource::ParserPlaceholder>(
        ExpandedModuleCallCheckInput::BindingUses {
            binding_use_report: &report,
            compositions: &[],
        },
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    assert_eq!(
        authored.len(),
        1,
        "unexpected authored findings: {authored:#?}"
    );
    assert_eq!(
        expanded.len(),
        1,
        "unexpected expanded findings: {expanded:#?}"
    );
    assert_ne!(
        authored[0].message().to_string(),
        expanded[0].message().to_string(),
        "the fixture must exercise display-label-independent identity",
    );

    let expected_anchor =
        ModuleCallFindingAnchor::new(report.findings()[0].use_site.location().clone());
    assert_eq!(authored[0].anchor(), &expected_anchor);
    assert_eq!(expanded[0].anchor(), &expected_anchor);
    let merged = merge_module_call_findings([authored, expanded]);
    assert_eq!(
        merged.len(),
        1,
        "one call/argument/typed error must survive both passes only once: {merged:#?}",
    );
}

#[test]
fn check_module_calls_defers_compatible_binding_refs_to_binding_use_authority() {
    use crate::parser::ArgumentParameter;

    let parsed = crate::parser::parse(
        "let name = 'compatible'\nlet inner = use { source = './inner' }\ninner { name = name }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("fixture must parse");
    let imported_modules = HashMap::from([(
        "inner".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "name".to_string(),
                static_type: consumer_type(TypeExpr::String),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let findings = module_call_findings_for_test(
        &parsed,
        &imported_modules,
        &LoadedProviderContext::empty_for_test(),
    );
    assert!(
        findings.is_empty(),
        "compatible binding-owned module arguments must not be rechecked: {findings:#?}"
    );
}

#[test]
fn provider_custom_type_validation_gets_api_spelling_at_every_type_expr_boundary() {
    use crate::binding_index::ResolvedBindings;
    use crate::parser::{ArgumentParameter, AttributeParameter};
    use std::sync::{Arc, Mutex};

    let identity = TypeIdentity::new(Some("test"), ["widget"], "Mode");
    let schema_type = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "test",
            identity.clone(),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let enum_type = AttributeType::enum_(
        identity.clone(),
        Some(vec!["API_VALUE".to_string()]),
        vec![("API_VALUE".to_string(), "dsl_value".to_string())],
        None,
        None,
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_by_validator = Arc::clone(&seen);
    let declaration = enum_type.clone();
    let config = loaded_context_for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                "test",
                identity,
                crate::schema::RefinedScalarKind::String,
            )]),
            Box::new(move |target, value| {
                let canonical =
                    crate::provider::canonicalize_custom_type_value(&declaration, value)
                        .map_err(|error| {
                            crate::parser::fixture_invalid_custom_type_value(
                                target,
                                error.into_type_error(),
                            )
                        })?
                        .expect("concrete enum value must canonicalize");
                seen_by_validator
                    .lock()
                    .expect("validator observation lock")
                    .push(canonical.to_string());
                Ok(())
            }),
        )
        .with_declarations_for_test(BTreeMap::from([("test".into(), vec![enum_type.clone()])])),
        resource_types: HashSet::from([("test".to_string(), "widget".to_string())]),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    });
    let authored = || Value::Concrete(ConcreteValue::enum_identifier("test.widget.Mode.dsl_value"));
    let module_parsed = crate::parser::parse_and_resolve(
        "let child = use { source = './child' }\nchild { mode = test.widget.Mode.dsl_value }",
    )
    .unwrap();
    let imported_modules = HashMap::from([(
        "child".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "mode".to_string(),
                static_type: consumer_type(schema_type.clone()),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    assert!(
        module_call_findings_for_test(&module_parsed, &imported_modules, &config).is_empty(),
        "module argument must validate"
    );

    validate_attribute_params(
        &[AttributeParameter::dummy(
            "mode".to_string(),
            Some(schema_type.clone()),
            Some(authored()),
        )],
        &config,
    )
    .expect("attribute parameter must validate");

    let exports = [RuntimeInferredExportParam {
        source: Default::default(),
        name: "mode".to_string(),
        static_type: crate::static_type::ExportStaticType::Declared(local_type(schema_type)),
        value: Some(
            authored()
                .resolve_root(&RootScope::new())
                .into_expanded()
                .expect("authored enum contains no parser placeholders"),
        ),
    }];
    crate::validation::deferred_types::with_session_for_test(&config, |session| {
        check_export_values_pre_apply(&exports, &ResolvedBindings::default(), session)
    })
    .expect("export must validate");

    let parsed = crate::parser::parse(
        r#"
            fn consume(mode: test.widget.Mode) { "ok" }
            let result = mock.test.resource {
                name = consume(test.widget.Mode.dsl_value)
            }
        "#,
        &config,
    )
    .expect("function argument must validate");
    assert!(
        matches!(
            parsed.resources[0].get_attr("name"),
            Some(Value::Concrete(ConcreteValue::String(value))) if value == "ok"
        ),
        "the static function call must be evaluated: {:?}",
        parsed.resources[0].get_attr("name"),
    );

    assert_eq!(
        *seen.lock().expect("validator observation lock"),
        ["API_VALUE", "API_VALUE", "API_VALUE", "API_VALUE"],
        "every TypeExpr consumer must resolve DSL spelling before calling the provider",
    );
}

#[test]
fn attribute_param_ref_type_mismatch_detected() {
    use crate::parser::AttributeParameter;

    // Build a resource with schema: role_name is String, arn is IamRoleArn (Custom)
    let role = Resource::with_provider("awscc", "iam.role", "github-role", None)
        .with_binding("role")
        .with_attribute(
            "role_name",
            Value::Concrete(ConcreteValue::String("my-role".into())),
        )
        .with_attribute(
            "arn",
            Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:role/my-role".into(),
            )),
        );

    let mut role_schema = PatternResourceSchema::new("iam.role");
    role_schema = role_schema.attribute(
        PatternAttributeSchema::optional("role_name", PatternAttributeType::string()).build(),
    );
    role_schema = role_schema.attribute(
        PatternAttributeSchema::optional(
            "arn",
            PatternAttributeType::refined_string_with_validator(
                TypeIdentity::bare("IamRoleArn"),
                None,
                None,
                crate::schema::legacy_validator(|_| Ok(())),
                None,
            ),
        )
        .build(),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        role_schema
            .validate()
            .expect("schema fixture must be valid"),
    );

    let mut parsed = empty_parsed();
    parsed.resources.push(role); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    // Attribute param: role_arn: iam_role_arn = role.role_name (MISMATCH: String vs iam_role_arn)
    let params_mismatch = vec![AttributeParameter::dummy(
        "role_arn".to_string(),
        Some(crate::parser::bare_type_for_test(
            "iam_role_arn".to_string(),
        )),
        Some(Value::resource_ref(
            "role".to_string(),
            "role_name".to_string(),
            vec![],
        )),
    )];

    let result = validate_attribute_param_ref_types_with_bindings(&params_mismatch, &bindings);
    assert!(
        result.is_err(),
        "Should reject String assigned to iam_role_arn"
    );
    let err = result.unwrap_err();
    assert!(err.contains("type mismatch"), "Error: {err}");
    assert!(err.contains("IamRoleArn"), "Error: {err}");

    // Attribute param: role_arn: iam_role_arn = role.arn (MATCH: IamRoleArn matches iam_role_arn)
    let params_match = vec![AttributeParameter::dummy(
        "role_arn".to_string(),
        Some(crate::parser::bare_type_for_test(
            "iam_role_arn".to_string(),
        )),
        Some(Value::resource_ref(
            "role".to_string(),
            "arn".to_string(),
            vec![],
        )),
    )];

    let result = validate_attribute_param_ref_types_with_bindings(&params_match, &bindings);
    assert!(
        result.is_ok(),
        "Should accept IamRoleArn assigned to iam_role_arn"
    );
}

#[test]
fn attribute_param_ref_type_resolves_terminal_schema_ref() {
    use crate::parser::AttributeParameter;

    let source = Resource::with_provider("test", "source.Item", "source", None)
        .with_binding("source")
        .with_attribute(
            "value",
            Value::Concrete(ConcreteValue::String("text".into())),
        );
    let source_schema = PatternResourceSchema::new("source.Item")
        .attribute(
            PatternAttributeSchema::optional("value", PatternAttributeType::ref_("X".to_string()))
                .build(),
        )
        .with_def("X", PatternAttributeType::string());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let mut parsed = empty_parsed();
    parsed.resources.push(source); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let params = vec![AttributeParameter::dummy(
        "value".to_string(),
        Some(TypeExpr::String),
        Some(Value::resource_ref(
            "source".to_string(),
            "value".to_string(),
            vec![],
        )),
    )];

    let result = validate_attribute_param_ref_types_with_bindings(&params, &bindings);
    assert!(
        result.is_ok(),
        "terminal Ref(X) should resolve to String for module-param compatibility: {result:?}",
    );
}

#[test]
fn attribute_param_ref_existence_checks_composition_attribute_membership_only() {
    use crate::parser::AttributeParameter;

    let params = vec![
        AttributeParameter::dummy(
            "valid".to_string(),
            None,
            Some(Value::resource_ref(
                "instance".to_string(),
                "target_group_arn".to_string(),
                vec!["runtime_field".to_string()],
            )),
        ),
        AttributeParameter::dummy(
            "invalid".to_string(),
            None,
            Some(Value::resource_ref(
                "instance".to_string(),
                "target_group_ar".to_string(),
                vec![],
            )),
        ),
    ];
    let mut parsed = empty_parsed();
    parsed
        .compositions
        .push(make_composition("instance", &["target_group_arn"]));
    let schemas = SchemaRegistry::new();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let err = validate_attribute_param_ref_types_with_bindings(&params, &bindings).unwrap_err();

    assert_eq!(
        err.lines().count(),
        1,
        "declared composition attr should pass without segment narrowing: {err}",
    );
    assert!(
        err.contains("attribute 'invalid': unknown attribute 'target_group_ar' on 'instance'"),
        "expected composition membership error, got: {err}",
    );
}

#[test]
fn attribute_param_ref_type_checks_composition_attribute_membership_only() {
    use crate::parser::AttributeParameter;

    let params = vec![
        AttributeParameter::dummy(
            "valid".to_string(),
            Some(crate::parser::bare_type_for_test(
                "unrelated_type".to_string(),
            )),
            Some(Value::resource_ref(
                "instance".to_string(),
                "target_group_arn".to_string(),
                vec!["runtime_field".to_string()],
            )),
        ),
        AttributeParameter::dummy(
            "invalid".to_string(),
            Some(crate::parser::bare_type_for_test(
                "unrelated_type".to_string(),
            )),
            Some(Value::resource_ref(
                "instance".to_string(),
                "target_group_ar".to_string(),
                vec![],
            )),
        ),
    ];
    let mut parsed = empty_parsed();
    parsed
        .compositions
        .push(make_composition("instance", &["target_group_arn"]));
    let schemas = SchemaRegistry::new();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let err = validate_attribute_param_ref_types_with_bindings(&params, &bindings).unwrap_err();

    assert_eq!(
        err.lines().count(),
        1,
        "declared composition attr should skip type compatibility and narrowing: {err}",
    );
    assert!(
        err.contains("attribute 'invalid': unknown attribute 'target_group_ar' on 'instance'"),
        "expected composition membership error, got: {err}",
    );
}

#[test]
fn post_expansion_composition_argument_schema_types_use_directional_relation() {
    let schema_type = |path: &str| TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", path, "Id"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let mut source = make_composition("inner", &[]);
    source.signature.attributes.insert(
        "security_group_id".to_string(),
        LiveCompositionAttribute::from_value(
            Value::Concrete(ConcreteValue::String("sg-123".into())),
            Some(consumer_type(schema_type("ec2.SecurityGroup"))),
        ),
    );
    let mut consumer = make_composition("consumer", &[]);
    consumer.signature.arguments.insert(
        "vpc_id".to_string(),
        LiveCompositionArgument::from_value(
            Value::resource_ref("inner".to_string(), "security_group_id".to_string(), vec![]),
            consumer_type(schema_type("ec2.Vpc")),
            ArgumentOrigin::Defaulted,
        ),
    );
    let mut parsed = empty_parsed();
    parsed.compositions.extend([source, consumer]);
    let parsed = parsed.resolve_root(&RootScope::new());
    let (context, schemas) = string_refinement_fixture();
    let bindings = crate::binding_index::BindingIndex::from_parsed(&parsed, &schemas, &context);
    let errors = validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings);
    let err = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        err.contains("module call 'consumer': argument 'vpc_id'")
            && err.contains("expected aws.ec2.Vpc.Id")
            && err.contains("got aws.ec2.SecurityGroup.Id")
            && err.contains("from inner.security_group_id"),
        "expanded composition outputs must use the shared source-to-sink relation: {err}",
    );
}

#[test]
fn composition_ref_error_dedup_uses_the_full_call_path() {
    fn call(source: &str, label: &str) -> CompositionCall {
        CompositionCall {
            id: crate::parser::ModuleCallId::test_dummy_in(source),
            module_name: label.to_string(),
            binding: Some(label.into()),
            instance: label.to_string(),
            module_source: None,
            module_directory: None,
            loop_origin: None,
        }
    }

    let schema_type = |path: &str| TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", path, "Id"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let mut source = make_composition("source", &[]);
    source.signature.attributes.insert(
        "group_id".to_string(),
        LiveCompositionAttribute::from_value(
            Value::Concrete(ConcreteValue::String("sg-123".into())),
            Some(consumer_type(schema_type("ec2.SecurityGroup"))),
        ),
    );

    let root = call("root.crn", "root");
    let immediate = call("immediate.crn", "inner");
    let make_consumer = |middle_source: &str| {
        let mut consumer = make_composition("inner", &[]);
        consumer.call_path = CompositionCallPath::direct(immediate.clone())
            .prepend(call(middle_source, "middle"))
            .prepend(root.clone());
        consumer.signature.arguments.insert(
            "vpc_id".to_string(),
            LiveCompositionArgument::from_value(
                Value::resource_ref("source", "group_id", vec![]),
                consumer_type(schema_type("ec2.Vpc")),
                ArgumentOrigin::Defaulted,
            ),
        );
        consumer
    };
    let compositions = vec![
        source,
        make_consumer("middle-a.crn"),
        make_consumer("middle-b.crn"),
    ];
    let mut parsed = empty_parsed();
    parsed.compositions = compositions;
    let parsed = parsed.resolve_root(&RootScope::new());
    let (_context, schemas) = string_refinement_fixture();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let errors = validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings);

    assert_eq!(
        errors
            .iter()
            .filter(|error| matches!(error, CompositionRefError::ModuleCall(_)))
            .count(),
        2,
        "paths with the same root and immediate call but different intermediates are distinct: {errors:#?}",
    );
}

#[test]
fn post_expansion_composition_attribute_typo_is_reported_once() {
    let source = make_composition("inner", &["actual"]);
    let mut consumer = make_composition("consumer", &[]);
    consumer.signature.attributes.insert(
        "x".to_string(),
        LiveCompositionAttribute::from_value(
            Value::resource_ref("inner".to_string(), "typo".to_string(), vec![]),
            Some(consumer_type(TypeExpr::String)),
        ),
    );
    let mut parsed = empty_parsed();
    parsed.compositions.extend([source, consumer]);
    let parsed = parsed.resolve_root(&RootScope::new());
    let schemas = SchemaRegistry::new();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let errors = validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings);
    let err = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        errors.len(),
        1,
        "a typed attribute reference must be visited exactly once: {err}",
    );
    assert!(
        err.contains("attribute 'x': unknown attribute 'typo' on 'inner'"),
        "expected the expanded composition surface diagnostic, got: {err}",
    );
}

#[test]
fn post_expansion_module_attribute_error_is_per_declaration_and_names_module() {
    let schema_type = |path: &str| TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", path, "Id"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let mut parsed = empty_parsed();
    for instance in ["one", "two", "three"] {
        let source_binding = format!("source_{instance}");
        let mut source = make_composition(&source_binding, &[]);
        source.signature.attributes.insert(
            "group_id".to_string(),
            LiveCompositionAttribute::from_value(
                Value::Concrete(ConcreteValue::String("sg-123".into())),
                Some(consumer_type(schema_type("ec2.SecurityGroup"))),
            ),
        );
        parsed.compositions.push(source);
        let mut consumer = make_composition(instance, &[]);
        set_composition_module(&mut consumer, "bad_module");
        consumer.signature.attributes.insert(
            "bad".to_string(),
            LiveCompositionAttribute::from_value(
                Value::resource_ref(source_binding, "group_id".to_string(), vec![]),
                Some(consumer_type(schema_type("ec2.Vpc"))),
            ),
        );
        parsed.compositions.push(consumer);
    }
    let parsed = parsed.resolve_root(&RootScope::new());
    let (_context, schemas) = string_refinement_fixture();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let errors = validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings);
    let declaration_errors: Vec<_> = errors
        .iter()
        .map(ToString::to_string)
        .filter(|error| error.contains("attribute 'bad'"))
        .collect();

    assert_eq!(declaration_errors.len(), 1, "{declaration_errors:?}");
    assert!(
        declaration_errors[0].contains("module 'bad_module'"),
        "the declaration diagnostic must identify its module: {declaration_errors:?}",
    );
}

#[test]
fn module_attribute_errors_deduplicate_directory_spellings_with_both_call_paths() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("modules/m")).unwrap();
    std::fs::create_dir(root.path().join("a")).unwrap();
    let mut parsed = empty_parsed();
    let mut source = make_composition("source", &[]);
    source.signature.attributes.insert(
        "value".to_string(),
        LiveCompositionAttribute::from_value(
            Value::Concrete(ConcreteValue::String("text".into())),
            Some(consumer_type(TypeExpr::String)),
        ),
    );
    parsed.compositions.push(source);
    for (instance, directory) in [
        ("one", root.path().join("modules/m")),
        ("two", root.path().join("a/../modules/m")),
    ] {
        let mut consumer = make_composition(instance, &[]);
        let mut call = consumer.call_path.immediate().clone();
        call.module_directory = Some(crate::paths::ResolvedPath::new(directory).unwrap());
        consumer.call_path = CompositionCallPath::direct(call);
        consumer.signature.attributes.insert(
            "bad".to_string(),
            LiveCompositionAttribute::from_value(
                Value::resource_ref("source".to_string(), "value".to_string(), vec![]),
                Some(consumer_type(TypeExpr::Bool)),
            ),
        );
        parsed.compositions.push(consumer);
    }
    let parsed = parsed.resolve_root(&RootScope::new());
    let schemas = SchemaRegistry::new();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let errors = validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings);
    assert_eq!(errors.len(), 1, "{errors:#?}");
    let CompositionRefError::Attribute(error) = &errors[0] else {
        panic!("expected a module declaration error: {errors:#?}");
    };
    let instances: Vec<_> = error
        .call_paths
        .iter()
        .map(|path| path.immediate().instance.as_str())
        .collect();
    assert_eq!(instances, ["one", "two"]);
}

#[test]
fn post_expansion_module_attribute_dedup_keeps_distinct_errors() {
    let security_group_id = TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", "ec2.SecurityGroup", "Id"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let mut parsed = empty_parsed();

    for instance in ["one", "two", "three"] {
        let source_binding = format!("{instance}.src");
        let mut source = make_composition(&source_binding, &[]);
        source.signature.attributes.insert(
            "group_id".to_string(),
            LiveCompositionAttribute::from_value(
                Value::Concrete(ConcreteValue::String("sg-123".into())),
                Some(consumer_type(security_group_id.clone())),
            ),
        );
        parsed.compositions.push(source);

        let mut consumer = make_composition(instance, &[]);
        set_composition_module(&mut consumer, "bad_module");
        consumer.signature.attributes.insert(
            "both".to_string(),
            LiveCompositionAttribute::from_value(
                Value::Concrete(ConcreteValue::List(vec![
                    Value::resource_ref(source_binding.clone(), "group_id".to_string(), vec![]),
                    Value::resource_ref(source_binding, "nonexistent_attr".to_string(), vec![]),
                ])),
                Some(consumer_type(TypeExpr::List(Box::new(TypeExpr::Bool)))),
            ),
        );
        parsed.compositions.push(consumer);
    }
    let parsed = parsed.resolve_root(&RootScope::new());

    let (_context, schemas) = string_refinement_fixture();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let errors = validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings);
    let declaration_errors: Vec<_> = errors
        .iter()
        .map(ToString::to_string)
        .filter(|error| error.contains("attribute 'both'"))
        .collect();

    assert_eq!(
        declaration_errors.len(),
        2,
        "each distinct authored failure must survive exactly once: {declaration_errors:?}",
    );
    assert_eq!(
        declaration_errors
            .iter()
            .filter(|error| error.contains("type mismatch"))
            .count(),
        1,
        "the repeated type mismatch must be deduplicated per declaration: {declaration_errors:?}",
    );
    assert_eq!(
        declaration_errors
            .iter()
            .filter(|error| error.contains("unknown attribute 'nonexistent_attr'"))
            .count(),
        1,
        "the repeated unknown-attribute error must be deduplicated per declaration: {declaration_errors:?}",
    );
}

#[test]
fn post_expansion_anonymous_call_error_uses_module_label() {
    let schema_type = |path: &str| TypeExpr::SchemaType {
        owned: crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", path, "Id"),
            crate::schema::RefinedScalarKind::String,
        )
        .unbounded_witness_for_test(),
    };
    let mut source = make_composition("source", &[]);
    source.signature.attributes.insert(
        "group_id".to_string(),
        LiveCompositionAttribute::from_value(
            Value::Concrete(ConcreteValue::String("sg-123".into())),
            Some(consumer_type(schema_type("ec2.SecurityGroup"))),
        ),
    );
    let synthetic_instance = "needs_0101081440007054";
    let mut consumer = make_composition(synthetic_instance, &[]);
    consumer.binding = None;
    set_composition_module(&mut consumer, "needs");
    consumer.signature.arguments.insert(
        "vpc_id".to_string(),
        LiveCompositionArgument::from_value(
            Value::resource_ref("source".to_string(), "group_id".to_string(), vec![]),
            consumer_type(schema_type("ec2.Vpc")),
            ArgumentOrigin::Defaulted,
        ),
    );
    let mut parsed = empty_parsed();
    parsed.compositions.extend([source, consumer]);
    let parsed = parsed.resolve_root(&RootScope::new());
    let (_context, schemas) = string_refinement_fixture();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let errors = validate_composition_ref_types_with_bindings(&parsed.compositions, &bindings);
    let rendered = errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        rendered.contains("module call 'needs (anonymous call)'"),
        "{rendered}"
    );
    assert!(!rendered.contains(synthetic_instance), "{rendered}");
}

#[test]
fn module_call_argument_checks_each_ref_inside_a_typed_list() {
    use crate::parser::ArgumentParameter;

    let subnet_id = PatternAttributeType::refined_string(
        Some(TypeIdentity::from_schema_type("aws", "ec2.Subnet", "Id")),
        None,
        None,
        None,
    );
    let security_group_id = PatternAttributeType::refined_string(
        Some(TypeIdentity::from_schema_type(
            "aws",
            "ec2.SecurityGroup",
            "Id",
        )),
        None,
        None,
        None,
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("ec2.Subnet")
            .attribute(PatternAttributeSchema::optional("subnet_id", subnet_id).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "aws",
        (PatternResourceSchema::new("ec2.SecurityGroup")
            .attribute(PatternAttributeSchema::optional("group_id", security_group_id).build()))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut parsed = empty_parsed();
    parsed
        .resources
        .push(Resource::with_provider("aws", "ec2.Subnet", "a", None).with_binding("a"));
    parsed
        .resources
        .push(Resource::with_provider("aws", "ec2.SecurityGroup", "web", None).with_binding("web"));
    let context = context_with_accepting_custom_type_resolver(HashSet::from([
        crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", "ec2.Subnet", "Id"),
            crate::schema::RefinedScalarKind::String,
        ),
        crate::parser::OwnedTypeIdentity::provider(
            "aws",
            TypeIdentity::from_schema_type("aws", "ec2.SecurityGroup", "Id"),
            crate::schema::RefinedScalarKind::String,
        ),
    ]));
    let bindings = crate::binding_index::BindingIndex::from_parsed(&parsed, &schemas, &context);

    let call = ModuleCall::test(
        "consumer".to_string(),
        Some("instance".to_string()),
        module_arguments([(
            "subnet_ids".to_string(),
            Value::Concrete(ConcreteValue::List(vec![
                Value::resource_ref("a".to_string(), "subnet_id".to_string(), vec![]),
                Value::resource_ref("web".to_string(), "group_id".to_string(), vec![]),
            ])),
        )]),
    );
    let imported_modules = HashMap::from([(
        "consumer".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "subnet_ids".to_string(),
                static_type: consumer_type(TypeExpr::List(Box::new(TypeExpr::SchemaType {
                    owned: crate::parser::OwnedTypeIdentity::provider(
                        "aws",
                        TypeIdentity::from_schema_type("aws", "ec2.Subnet", "Id"),
                        crate::schema::RefinedScalarKind::String,
                    )
                    .unbounded_witness_for_test(),
                }))),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let errors = validate_module_call_argument_ref_types_with_bindings(
        &[call],
        &imported_modules,
        &HashSet::new(),
        &bindings,
    );

    assert_eq!(
        errors.len(),
        1,
        "only the SecurityGroup Id should fail the list element sink: {errors:?}",
    );
    let err = errors[0].to_string();
    assert!(
        err.contains("argument 'subnet_ids'")
            && err.contains("expected aws.ec2.Subnet.Id")
            && err.contains("got aws.ec2.SecurityGroup.Id")
            && err.contains("from web.group_id"),
        "nested list refs must use the element type as their sink: {err}",
    );
}

#[test]
fn module_call_argument_ref_type_widens_int_source_to_float_sink() {
    use crate::parser::ArgumentParameter;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        (PatternResourceSchema::new("counter").attribute(
            PatternAttributeSchema::optional("count", PatternAttributeType::int()).build(),
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let mut parsed = empty_parsed();
    parsed
        .resources
        .push(Resource::with_provider("mock", "counter", "source", None).with_binding("source"));
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let call = ModuleCall::test(
        "consumer".to_string(),
        None,
        module_arguments([(
            "number".to_string(),
            Value::resource_ref("source", "count", Vec::new()),
        )]),
    );
    let imported_modules = HashMap::from([(
        "consumer".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![ArgumentParameter {
                name: "number".to_string(),
                static_type: consumer_type(TypeExpr::Float),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);

    let errors = validate_module_call_argument_ref_types_with_bindings(
        &[call],
        &imported_modules,
        &HashSet::new(),
        &bindings,
    );

    assert!(
        errors.is_empty(),
        "an Int-typed resource attribute must widen into a Float module argument: {errors:?}"
    );
}

#[test]
fn attribute_param_ref_types_flags_unknown_attribute_inside_nested_struct() {
    use crate::parser::AttributeParameter;

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "vpc_id",
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );

    let mut vpc_schema = PatternResourceSchema::new("ec2.Vpc");
    vpc_schema = vpc_schema.attribute(
        PatternAttributeSchema::optional("vpc_id", PatternAttributeType::string()).build(),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        vpc_schema.validate().expect("schema fixture must be valid"),
    );

    let mut nested = IndexMap::new();
    nested.insert(
        "id".to_string(),
        Value::resource_ref("vpc".to_string(), "bad_attr".to_string(), vec![]),
    );
    let params = vec![AttributeParameter::dummy(
        "network".to_string(),
        Some(crate::parser::bare_type_for_test("vpc_id".to_string())),
        Some(Value::Concrete(ConcreteValue::Map(nested))),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let err = validate_attribute_param_ref_types_with_bindings(&params, &bindings).unwrap_err();
    assert!(
        err.contains("attribute 'network': unknown attribute 'bad_attr' on 'vpc'"),
        "expected nested attribute-param ResourceRef error, got: {err}",
    );
}

#[test]
fn attribute_param_ref_types_flags_unknown_attribute_through_wait_binding() {
    use crate::parser::AttributeParameter;

    let cert = Resource::with_provider("aws", "acm.Certificate", "cert", None)
        .with_binding("cert")
        .with_attribute(
            "status",
            Value::Concrete(ConcreteValue::String("pending".into())),
        );

    let mut cert_schema = PatternResourceSchema::new("acm.Certificate");
    cert_schema = cert_schema.attribute(
        PatternAttributeSchema::optional("certificate_arn", PatternAttributeType::string()).build(),
    );
    cert_schema = cert_schema.attribute(
        PatternAttributeSchema::optional("status", PatternAttributeType::string()).build(),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        cert_schema
            .validate()
            .expect("schema fixture must be valid"),
    );

    let params = vec![AttributeParameter::dummy(
        "certificate_arn".to_string(),
        Some(crate::parser::bare_type_for_test("string".to_string())),
        Some(Value::resource_ref(
            "cert_issued".to_string(),
            "bad_attr".to_string(),
            vec![],
        )),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(cert); // allow: direct — fixture test inspection
    parsed.wait_bindings.push(WaitBinding {
        binding: RelativeBindingName::from("cert_issued"),
        target: RelativeBindingName::from("cert"),
        until_raw: "cert.status == 'issued'".to_string(),
        until_predicate: UntilPredicateAst {
            root: "cert".into(),
            attribute_segments: vec!["status".to_string()],
            rhs: RelativeRuntimeValue::Concrete(crate::resource::ConcreteValue::String(
                "issued".into(),
            )),
        },
        timeout_secs: None,
        depends_on: Default::default(),
        line: 1,
        declaration: Default::default(),
    });

    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let err = validate_attribute_param_ref_types_with_bindings(&params, &bindings).unwrap_err();
    assert!(
        err.contains("attribute 'certificate_arn': unknown attribute 'bad_attr' on 'cert_issued'"),
        "expected attribute-param wait binding unknown attribute error, got: {err}",
    );
}

#[test]
fn attribute_param_ref_types_does_not_misreport_type_mismatch_for_nested_ref_to_existing_attribute()
{
    use crate::parser::AttributeParameter;

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "cidr_block",
            Value::Concrete(ConcreteValue::String("10.0.0.0/16".into())),
        );

    let mut vpc_schema = PatternResourceSchema::new("ec2.Vpc");
    vpc_schema = vpc_schema.attribute(
        PatternAttributeSchema::optional("cidr_block", PatternAttributeType::string()).build(),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        vpc_schema.validate().expect("schema fixture must be valid"),
    );

    let mut nested = IndexMap::new();
    nested.insert(
        "id".to_string(),
        Value::resource_ref("vpc".to_string(), "cidr_block".to_string(), vec![]),
    );
    let params = vec![AttributeParameter::dummy(
        "network".to_string(),
        Some(crate::parser::bare_type_for_test("vpc_id".to_string())),
        Some(Value::Concrete(ConcreteValue::Map(nested))),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let result = validate_attribute_param_ref_types_with_bindings(&params, &bindings);
    assert!(
        result.is_ok(),
        "nested refs should be existence-checked only, got: {result:?}",
    );
}

fn validate_attribute_params_against_target_group(
    params: &[crate::parser::AttributeParameter],
) -> Result<(), String> {
    let target_group = Resource::with_provider("awscc", "elbv2.TargetGroup", "app", None)
        .with_binding("target_group")
        .with_attribute("name", Value::Concrete(ConcreteValue::String("app".into())));
    let target_group_schema = PatternResourceSchema::new("elbv2.TargetGroup")
        .attribute(PatternAttributeSchema::optional("name", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional("target_group_arn", PatternAttributeType::string())
                .build(),
        );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        target_group_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let mut parsed = empty_parsed();
    parsed.resources.push(target_group); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    validate_attribute_param_ref_types_with_bindings(params, &bindings)
}

fn validate_attribute_params_against_target_group_tags(
    params: &[crate::parser::AttributeParameter],
) -> Result<(), String> {
    let target_group = Resource::with_provider("awscc", "elbv2.TargetGroup", "app", None)
        .with_binding("target_group")
        .with_attribute("tags", Value::Concrete(ConcreteValue::Map(IndexMap::new())));
    let target_group_schema = make_schema(
        "elbv2.TargetGroup",
        vec![(
            "tags",
            PatternAttributeType::struct_(
                "TargetGroupTags",
                vec![
                    PatternStructField::optional(
                        "environment",
                        PatternAttributeType::refined_string(
                            Some(TypeIdentity::bare("EnvironmentName")),
                            None,
                            None,
                            None,
                        ),
                    )
                    .build(),
                    PatternStructField::optional("bad_fields", PatternAttributeType::string())
                        .build(),
                ],
            ),
        )],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        target_group_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let mut parsed = empty_parsed();
    parsed.resources.push(target_group); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    validate_attribute_param_ref_types_with_bindings(params, &bindings)
}

#[test]
fn attribute_param_nested_unknown_field_reports_suggestion() {
    use crate::parser::AttributeParameter;

    let params = vec![AttributeParameter::dummy(
        "x".to_string(),
        None,
        Some(Value::resource_ref(
            "target_group".to_string(),
            "tags".to_string(),
            vec!["bad_field".to_string()],
        )),
    )];

    let err = validate_attribute_params_against_target_group_tags(&params).unwrap_err();

    assert!(
        err.contains("unknown field 'bad_field' on struct 'TargetGroupTags'"),
        "error: {err}",
    );
    assert!(
        err.contains("reference target_group.tags.bad_field"),
        "error should contain the full reference path: {err}",
    );
    assert!(
        err.contains("Did you mean 'bad_fields'?"),
        "error should suggest the closest struct field: {err}",
    );
}

#[test]
fn attribute_param_nested_known_field_is_valid() {
    use crate::parser::AttributeParameter;

    let params = vec![AttributeParameter::dummy(
        "x".to_string(),
        None,
        Some(Value::resource_ref(
            "target_group".to_string(),
            "tags".to_string(),
            vec!["environment".to_string()],
        )),
    )];

    let result = validate_attribute_params_against_target_group_tags(&params);

    assert!(
        result.is_ok(),
        "known nested field should validate: {result:?}"
    );
}

#[test]
fn attribute_param_nested_simple_annotation_uses_leaf_type() {
    use crate::parser::AttributeParameter;

    let params = vec![AttributeParameter::dummy(
        "x".to_string(),
        Some(crate::parser::bare_type_for_test(
            "environment_name".to_string(),
        )),
        Some(Value::resource_ref(
            "target_group".to_string(),
            "tags".to_string(),
            vec!["environment".to_string()],
        )),
    )];

    let result = validate_attribute_params_against_target_group_tags(&params);

    assert!(
        result.is_ok(),
        "Simple annotation should compare against the narrowed leaf: {result:?}",
    );
}

#[test]
fn unannotated_attribute_param_flags_unknown_resource_attribute_with_suggestion() {
    use crate::parser::AttributeParameter;

    let params = vec![AttributeParameter::dummy(
        "target_group_arn".to_string(),
        None,
        Some(Value::resource_ref(
            "target_group".to_string(),
            "target_group_ar".to_string(),
            vec![],
        )),
    )];

    let err = validate_attribute_params_against_target_group(&params).unwrap_err();

    assert!(
        err.contains("unknown attribute 'target_group_ar'"),
        "error: {err}",
    );
    assert!(
        err.contains("Did you mean 'target_group_arn'?"),
        "error should suggest the schema attribute: {err}",
    );
}

#[test]
fn primitive_annotated_attribute_param_flags_unknown_resource_attribute() {
    use crate::parser::AttributeParameter;

    let params = vec![AttributeParameter::dummy(
        "target_group_arn".to_string(),
        Some(TypeExpr::String),
        Some(Value::resource_ref(
            "target_group".to_string(),
            "arn".to_string(),
            vec![],
        )),
    )];

    let err = validate_attribute_params_against_target_group(&params).unwrap_err();

    assert!(err.contains("unknown attribute 'arn'"), "error: {err}");
}

#[test]
fn unannotated_attribute_param_flags_unknown_resource_attribute_in_nested_value() {
    use crate::parser::AttributeParameter;

    let params = vec![AttributeParameter::dummy(
        "target_group_arns".to_string(),
        None,
        Some(Value::Concrete(ConcreteValue::List(vec![
            Value::resource_ref("target_group".to_string(), "arn".to_string(), vec![]),
        ]))),
    )];

    let err = validate_attribute_params_against_target_group(&params).unwrap_err();

    assert!(err.contains("unknown attribute 'arn'"), "error: {err}");
}

#[test]
fn simple_annotated_plain_ref_reports_unknown_attribute_once() {
    use crate::parser::AttributeParameter;

    let params = vec![AttributeParameter::dummy(
        "target_group_arn".to_string(),
        Some(crate::parser::bare_type_for_test(
            "target_group_arn".to_string(),
        )),
        Some(Value::resource_ref(
            "target_group".to_string(),
            "target_group_ar".to_string(),
            vec![],
        )),
    )];

    let err = validate_attribute_params_against_target_group(&params).unwrap_err();

    assert_eq!(
        err.lines().count(),
        1,
        "the direct Simple path must not report the same missing attribute twice: {err}",
    );
    assert!(
        err.contains("Did you mean 'target_group_arn'?"),
        "the direct Simple path should suggest the schema attribute: {err}",
    );
}

fn resolve_and_validate_export_params(
    exports: &[crate::parser::InferredExportParam],
    config: &LoadedProviderContext,
) -> Result<(), String> {
    let exports = exports
        .iter()
        .cloned()
        .map(|export| {
            Ok::<_, crate::resource::LeftoverPlaceholder>(RuntimeInferredExportParam {
                source: Default::default(),
                name: export.name,
                static_type: export.static_type,
                value: export
                    .value
                    .map(|value| value.resolve_root(&RootScope::new()).into_expanded())
                    .transpose()?,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    resolve_and_validate_expanded_export_params(&exports, config)
}

fn resolve_and_validate_expanded_export_params(
    exports: &[RuntimeInferredExportParam],
    config: &LoadedProviderContext,
) -> Result<(), String> {
    crate::validation::deferred_types::with_session_for_test(config, |session| {
        check_export_values_pre_apply(
            exports,
            &crate::binding_index::ResolvedBindings::default(),
            session,
        )
    })
    .map(|_| ())
}

#[test]
fn unloaded_export_context_checks_builtins_but_cannot_check_provider_custom_types() {
    use crate::binding_index::ResolvedBindings;
    use crate::parser::InferredExportParam;

    let builtin: RuntimeInferredExportParam = InferredExportParam {
        source: Default::default(),
        name: "flag".to_string(),
        static_type: crate::static_type::ExportStaticType::Declared(TypeExpr::Bool),
        value: Some(RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "not-a-bool".into(),
        ))),
    };
    let error = crate::validation::deferred_types::with_session_for_test(
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        |session| {
            check_export_values_pre_apply_providerless(
                std::slice::from_ref(&builtin),
                &ResolvedBindings::default(),
                session,
            )
        },
    )
    .expect_err("built-in mismatch must still be diagnosed")
    .to_string();
    assert!(error.contains("expected Bool, got string"), "error={error}");

    let custom: RuntimeInferredExportParam = InferredExportParam {
        source: Default::default(),
        name: "identifiers".to_string(),
        static_type: crate::static_type::ExportStaticType::Declared(TypeExpr::List(Box::new(
            TypeExpr::SchemaType {
                owned: crate::parser::OwnedTypeIdentity::provider(
                    "mock",
                    TypeIdentity::bare("RecoveryProbe"),
                    crate::schema::RefinedScalarKind::String,
                )
                .unbounded_witness_for_test(),
            },
        ))),
        value: Some(RuntimeValue::Concrete(RuntimeConcreteValue::StringList(
            vec!["identifier".to_string().into()],
        ))),
    };
    crate::validation::deferred_types::with_session_for_test(
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        |session| {
            check_export_values_pre_apply_providerless(
                std::slice::from_ref(&custom),
                &ResolvedBindings::default(),
                session,
            )
        },
    )
    .expect("providerless export checks must skip only the custom-type leaf");

    let builtin_custom: RuntimeInferredExportParam = InferredExportParam {
        source: Default::default(),
        name: "cidr".to_string(),
        static_type: crate::static_type::ExportStaticType::Declared(
            crate::parser::bare_type_for_test("ipv4_cidr".to_string()),
        ),
        value: Some(RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "10.0.0.0/16".to_string().into(),
        ))),
    };
    crate::validation::deferred_types::with_session_for_test(
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        |session| {
            check_export_values_pre_apply_providerless(
                std::slice::from_ref(&builtin_custom),
                &ResolvedBindings::default(),
                session,
            )
        },
    )
    .expect("built-in custom types remain checkable without providers");
}

#[test]
fn validate_export_params_rejects_invalid_custom_type() {
    use crate::parser::InferredExportParam;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![InferredExportParam::for_test(
        "policy",
        crate::static_type::ExportStaticType::Declared(local_type(iam_policy_arn_type_expr())),
        Some(Value::Concrete(ConcreteValue::String("not-an-arn".into()))),
    )];
    let result = resolve_and_validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'policy'"), "err={err}");
    assert!(err.contains("invalid IAM policy ARN"), "err={err}");
}

#[test]
fn validate_export_params_rejects_invalid_list_element() {
    use crate::parser::InferredExportParam;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![InferredExportParam::for_test(
        "policies",
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::List(Box::new(
            iam_policy_arn_type_expr(),
        )))),
        Some(Value::Concrete(ConcreteValue::List(vec![
            Value::Concrete(ConcreteValue::String(
                "arn:aws:iam::123456789012:policy/valid".into(),
            )),
            Value::Concrete(ConcreteValue::String("garbage".into())),
        ]))),
    )];
    let result = resolve_and_validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'policies'"), "err={err}");
    assert!(err.contains("list element 1"), "err={err}");
}

#[test]
fn validate_export_params_accepts_valid_values() {
    use crate::parser::InferredExportParam;

    let config = context_with_iam_policy_arn_validator();
    let exports = vec![InferredExportParam::for_test(
        "policy",
        crate::static_type::ExportStaticType::Declared(local_type(iam_policy_arn_type_expr())),
        Some(Value::Concrete(ConcreteValue::String(
            "arn:aws:iam::123456789012:policy/admin".into(),
        ))),
    )];
    let result = resolve_and_validate_export_params(&exports, &config);
    assert!(result.is_ok());
}

#[test]
fn validate_export_params_skips_unchecked_inference() {
    use crate::parser::InferredExportParam;
    use crate::static_type::{
        ExportStaticType, InferredUncheckedReason, NotInferableReason, StaticType,
    };

    // Unchecked exports do not invent a real type for value validation.
    let config = loaded_context_for_test(ProviderContext::new(crate::EnvironmentSnapshot::empty()));
    let exports = vec![InferredExportParam::for_test(
        "raw",
        ExportStaticType::Inferred(StaticType::Unchecked(
            InferredUncheckedReason::NotInferable(NotInferableReason::MissingValue),
        )),
        Some(Value::Concrete(ConcreteValue::String("anything".into()))),
    )];
    let result = resolve_and_validate_export_params(&exports, &config);
    assert!(result.is_ok());
}

#[test]
fn validate_export_params_rejects_type_mismatch() {
    use crate::parser::InferredExportParam;

    let config = loaded_context_for_test(ProviderContext::new(crate::EnvironmentSnapshot::empty()));
    let exports = vec![InferredExportParam::for_test(
        "flag",
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Bool)),
        Some(Value::Concrete(ConcreteValue::String("not-a-bool".into()))),
    )];
    let result = resolve_and_validate_export_params(&exports, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.contains("export 'flag'"), "err={err}");
    assert!(err.contains("expected Bool"), "err={err}");
}

#[test]
fn validate_export_params_rejects_decrypt_result_as_secret_export() {
    let plaintext = "hunter2SECRET";
    let config = loaded_context_for_test(ProviderContext {
        decryptor: Some(std::sync::Arc::new(move |_, _| Ok(plaintext.to_string()))),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    });
    let value = crate::builtins::evaluate_builtin_with_config_to_value(
        "decrypt",
        &[RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "ciphertext".into(),
        ))],
        &config,
    )
    .expect("test decryptor");
    let exports = vec![RuntimeInferredExportParam {
        source: Default::default(),
        name: "count".to_string(),
        static_type: crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Int)),
        value: Some(value),
    }];

    let error = resolve_and_validate_expanded_export_params(&exports, &config)
        .expect_err("secret cannot be exported");

    assert_eq!(error, "export 'count' cannot hold a secret value");
    assert!(!error.contains(plaintext), "error={error}");
}

#[test]
fn validate_export_params_rejects_every_secret_value_shape_uniformly() {
    fn secret(value: RuntimeValue) -> RuntimeValue {
        RuntimeValue::Deferred(RuntimeDeferredValue::Secret(Box::new(value).into()))
    }

    let secret_string = || {
        secret(RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "topsecretXYZ".into(),
        )))
    };
    let builtin = crate::builtins::evaluate_builtin_to_value_for_tests("upper", &[secret_string()])
        .expect("upper(secret(...)) must evaluate");
    let config = loaded_context_for_test(ProviderContext {
        decryptor: Some(std::sync::Arc::new(|_, _| {
            Ok("decrypted-secret".to_string())
        })),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    });
    let decrypt = crate::builtins::evaluate_builtin_with_config_to_value(
        "decrypt",
        &[RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "ciphertext".into(),
        ))],
        &config,
    )
    .expect("test decryptor");
    let exports = vec![
        RuntimeInferredExportParam {
            source: Default::default(),
            name: "direct".into(),
            static_type: crate::static_type::ExportStaticType::Declared(local_type(
                TypeExpr::String,
            )),
            value: Some(secret_string()),
        },
        RuntimeInferredExportParam {
            source: Default::default(),
            name: "builtin".into(),
            static_type: crate::static_type::ExportStaticType::Declared(local_type(
                TypeExpr::String,
            )),
            value: Some(builtin),
        },
        RuntimeInferredExportParam {
            source: Default::default(),
            name: "interpolated".into(),
            static_type: crate::static_type::ExportStaticType::Declared(local_type(
                TypeExpr::String,
            )),
            value: Some(RuntimeValue::Deferred(RuntimeDeferredValue::Interpolation(
                vec![
                    RuntimeInterpolationPart::Literal("prefix-".into()),
                    RuntimeInterpolationPart::Expr(secret_string()),
                ],
            ))),
        },
        RuntimeInferredExportParam {
            source: Default::default(),
            name: "nested_list".into(),
            static_type: crate::static_type::ExportStaticType::Declared(local_type(
                TypeExpr::List(Box::new(TypeExpr::String)),
            )),
            value: Some(RuntimeValue::Concrete(RuntimeConcreteValue::List(vec![
                secret_string(),
            ]))),
        },
        RuntimeInferredExportParam {
            source: Default::default(),
            name: "nested_map".into(),
            static_type: crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Map(
                Box::new(TypeExpr::String),
            ))),
            value: Some(RuntimeValue::Concrete(RuntimeConcreteValue::Map(
                IndexMap::from([("key".into(), secret_string())]),
            ))),
        },
        RuntimeInferredExportParam {
            source: Default::default(),
            name: "decrypted".into(),
            static_type: crate::static_type::ExportStaticType::Declared(local_type(
                TypeExpr::String,
            )),
            value: Some(decrypt),
        },
    ];

    let error = resolve_and_validate_expanded_export_params(&exports, &config)
        .expect_err("secrets cannot be exported");

    for name in [
        "direct",
        "builtin",
        "interpolated",
        "nested_list",
        "nested_map",
        "decrypted",
    ] {
        assert!(
            error.contains(&format!("export '{name}' cannot hold a secret value")),
            "missing uniform error for {name}: {error}",
        );
    }
    assert!(!error.contains("topsecretXYZ"), "error={error}");
    assert!(!error.contains("decrypted-secret"), "error={error}");
}

#[test]
fn pre_apply_export_check_retains_expression_for_post_apply_resolution() {
    use crate::binding_index::{LivePreApplyInputs, ResolvedBindings};

    fn bindings(identifier: &str) -> ResolvedBindings {
        let resource = Resource::with_provider("mock", "test.resource", "resource", None)
            .with_binding("r")
            .with_attribute(
                "identifier",
                Value::Concrete(ConcreteValue::String(identifier.into())),
            )
            .resolve_root(&RootScope::new())
            .into_expanded(&crate::schema::SchemaRegistry::new())
            .expect("resource fixture contains no parser placeholder");
        ResolvedBindings::pre_apply_live(LivePreApplyInputs {
            managed: &[resource],
            compositions: &[],
            managed_current: &HashMap::new(),
            data_source_values: &crate::data_source_value::DataSourceValues::empty(),
            remote_bindings: &HashMap::new(),
            wait_aliases: &[],
        })
    }

    let exports = [RuntimeInferredExportParam {
        source: Default::default(),
        name: "id".into(),
        static_type: crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::String)),
        value: Some(RuntimeValue::root_resource_ref("r", "identifier", vec![])),
    }];
    let provider_context =
        loaded_context_for_test(ProviderContext::new(crate::EnvironmentSnapshot::empty()));
    let pre_apply =
        crate::validation::deferred_types::with_session_for_test(&provider_context, |session| {
            check_export_values_pre_apply(&exports, &bindings("alpha-id"), session)
        })
        .expect("pre-apply export check");
    assert_eq!(
        pre_apply.display_values()[0].value,
        Some(RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "alpha-id".into(),
        ))),
    );

    let post_apply =
        crate::validation::deferred_types::with_session_for_test(&provider_context, |session| {
            resolve_export_values_post_apply(&pre_apply, &bindings("beta-id"), session)
        })
        .expect("post-apply export resolution");
    assert_eq!(
        post_apply.as_slice()[0].value,
        Some(RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "beta-id".into(),
        ))),
    );
}

#[test]
fn type_relation_provider_scoped_subtype_flows_into_bare_annotation() {
    // #1874: a declared `Arn` sink accepts a provider-scoped KMS key ARN
    // source (narrower segments, same kind).
    let kms_key_arn = AttributeType::refined_string_with_validator(
        TypeIdentity::new(Some("aws"), ["kms", "Key"], "Arn"),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert!(schema_source_is_assignable_to_lifted_sink(
        &kms_key_arn,
        crate::schema::empty_defs_for_schema_walks(),
        &crate::parser::bare_type_for_test("arn".to_string()),
    ));
}

#[test]
fn type_relation_sibling_rejected_by_declared_sink() {
    // A declared `KmsKeyArn` sink rejects an IAM role ARN source.
    let iam_role_arn = AttributeType::refined_string_with_validator(
        TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn"),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert!(!schema_source_is_assignable_to_lifted_sink(
        &iam_role_arn,
        crate::schema::empty_defs_for_schema_walks(),
        &crate::parser::bare_type_for_test("kms_key_arn".to_string()),
    ));
}

#[test]
fn type_relation_resource_id_subtype_flows_into_bare_annotation() {
    // A declared `ResourceId` sink accepts VPC resource IDs (narrower
    // segments, same kind).
    let vpc_id = AttributeType::refined_string_with_validator(
        TypeIdentity::new(Some("aws"), ["ec2", "Vpc"], "ResourceId"),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert!(schema_source_is_assignable_to_lifted_sink(
        &vpc_id,
        crate::schema::empty_defs_for_schema_walks(),
        &crate::parser::bare_type_for_test("resource_id".to_string()),
    ));
}

#[test]
fn type_relation_resource_id_siblings_rejected() {
    // A declared `VpcId` sink rejects a SubnetId source.
    let subnet_id = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("SubnetId"),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert!(!schema_source_is_assignable_to_lifted_sink(
        &subnet_id,
        crate::schema::empty_defs_for_schema_walks(),
        &crate::parser::bare_type_for_test("vpc_id".to_string()),
    ));
}

#[test]
fn type_relation_exact_identity_match() {
    let arn = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Arn"),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    assert!(schema_source_is_assignable_to_lifted_sink(
        &arn,
        crate::schema::empty_defs_for_schema_walks(),
        &crate::parser::bare_type_for_test("arn".to_string()),
    ));
}

fn mutually_recursive_string_union_fixture() -> (
    AttributeType,
    std::collections::BTreeMap<String, AttributeType>,
) {
    (
        AttributeType::ref_("A"),
        std::collections::BTreeMap::from([
            (
                "A".to_string(),
                AttributeType::union(vec![AttributeType::ref_("B"), AttributeType::string()]),
            ),
            (
                "B".to_string(),
                AttributeType::union(vec![AttributeType::ref_("A"), AttributeType::string()]),
            ),
        ]),
    )
}

#[test]
fn string_compatible_predicate_uses_concrete_member_through_mutual_ref_cycle() {
    let (root, defs) = mutually_recursive_string_union_fixture();

    assert!(is_string_compatible_type(&root, &defs));
}

#[test]
fn directional_relation_finds_concrete_member_through_mutual_ref_cycle() {
    let (root, defs) = mutually_recursive_string_union_fixture();

    assert!(lifted_source_is_assignable_to_schema(
        &TypeExpr::String,
        &root,
        &defs,
    ));
}

#[test]
fn directional_relation_accepts_refined_string_through_mixed_union_string_member() {
    // Union sinks use ordinary sum-type assignability: one accepting member
    // is sufficient, regardless of other scalar alternatives.
    let mixed = AttributeType::union(vec![AttributeType::string(), AttributeType::int()]);
    assert!(lifted_source_is_assignable_to_schema(
        &crate::parser::bare_type_for_test("aws_account_id".to_string()),
        &mixed,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn lifted_simple_subtypes_into_plain_string() {
    // `Simple("aws_account_id")` is a particular kind of string;
    // the plain-`String` receiver wants any string, so the value
    // satisfies it. The reverse direction (plain `String` value
    // into a `Custom { semantic_name: AwsAccountId }` receiver) is
    // remains rejected by directional identity assignability. See #1874 and
    // #2643.
    assert!(lifted_source_is_assignable_to_schema(
        &crate::parser::bare_type_for_test("aws_account_id".to_string()),
        &AttributeType::string(),
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

// Issue #2663: a semantic-ARN refinement (`IamOidcProviderArn`) flowed
// into a `Union<Struct(Principal), String>` receiver — the canonical
// shape for IAM-style policy `principal` slots — was rejected because
// the existing `Union<plain String>` rule required *every* member to
// be a plain String. The Struct member made the union fall through to
// the catch-all reject. The Simple value is unambiguously string-shaped
// at runtime, and the Struct member only accepts map-shaped values, so
// the two members are shape-disjoint and the assignment is safe.
#[test]
fn lifted_simple_flows_into_union_struct_or_string() {
    use crate::schema::CompiledStructField as StructField;
    let principal_union = AttributeType::union(vec![
        AttributeType::struct_(
            "IamPolicyPrincipal".to_string(),
            vec![StructField::optional("federated", AttributeType::string()).build()],
        ),
        AttributeType::string(),
    ]);
    // Issue #2663 enumerates the full set of ARN refinements that
    // should reach the principal slot; pin each one against the same
    // receiver so the table is the spec.
    for name in [
        "arn",
        "iam_role_arn",
        "iam_policy_arn",
        "iam_oidc_provider_arn",
        "kms_key_arn",
    ] {
        assert!(
            lifted_source_is_assignable_to_schema(
                &crate::parser::bare_type_for_test(name.to_string()),
                &principal_union,
                crate::schema::empty_defs_for_schema_walks(),
            ),
            "Simple({name}) should be assignable to Union<Struct, String>"
        );
    }
}

// The removed compatibility predicate rejected this shape because another
// scalar branch exists. The shared directional relation treats a union as a
// sum type, so its plain String member is enough.
#[test]
fn directional_relation_accepts_refined_string_when_union_has_other_scalar() {
    use crate::schema::CompiledStructField as StructField;
    let mixed = AttributeType::union(vec![
        AttributeType::struct_(
            "Principal".to_string(),
            vec![StructField::optional("federated", AttributeType::string()).build()],
        ),
        AttributeType::string(),
        AttributeType::int(),
    ]);
    assert!(lifted_source_is_assignable_to_schema(
        &crate::parser::bare_type_for_test("iam_oidc_provider_arn".to_string()),
        &mixed,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

// A union without any plain-String member cannot accept a Simple value:
// even a List<String> member shapes its values as a list, not a string.
#[test]
fn lifted_simple_rejected_when_union_has_no_compatible_member() {
    use crate::schema::CompiledStructField as StructField;
    let no_string = AttributeType::union(vec![
        AttributeType::struct_(
            "Principal".to_string(),
            vec![StructField::optional("federated", AttributeType::string()).build()],
        ),
        AttributeType::list(AttributeType::string()),
    ]);
    assert!(!lifted_source_is_assignable_to_schema(
        &crate::parser::bare_type_for_test("iam_oidc_provider_arn".to_string()),
        &no_string,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

// Identified/enum peers do not invalidate an independently compatible plain
// String member under normal union-sink assignability.
#[test]
fn directional_relation_accepts_refined_string_with_string_shaped_union_peers() {
    let arn = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Arn"),
        None,
        None,
        crate::schema::legacy_validator(|_| Ok(())),
        None,
    );
    let with_custom = AttributeType::union(vec![AttributeType::string(), arn]);
    assert!(lifted_source_is_assignable_to_schema(
        &crate::parser::bare_type_for_test("iam_oidc_provider_arn".to_string()),
        &with_custom,
        crate::schema::empty_defs_for_schema_walks(),
    ));
    let with_enum = AttributeType::union(vec![
        AttributeType::string(),
        AttributeType::enum_(
            TypeIdentity::bare("Status"),
            Some(vec!["enabled".to_string(), "disabled".to_string()]),
            vec![],
            None,
            None,
        ),
    ]);
    assert!(lifted_source_is_assignable_to_schema(
        &crate::parser::bare_type_for_test("iam_oidc_provider_arn".to_string()),
        &with_enum,
        crate::schema::empty_defs_for_schema_walks(),
    ));
}

#[test]
fn validate_export_param_ref_types_map_accepts_compatible_types() {
    use crate::parser::InferredExportParam;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "organizations.account",
            vec![("account_id", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let registry_prod = Resource::with_provider("awscc", "organizations.account", "prod", None)
        .with_binding("registry_prod")
        .with_attribute(
            "account_id",
            Value::Concrete(ConcreteValue::String("111".into())),
        );

    let mut map_value = IndexMap::new();
    map_value.insert(
        "prod".to_string(),
        Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        ),
    );

    let exports = vec![InferredExportParam::for_test(
        "accounts",
        // declared as map(string), and values are String-typed — should pass
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Map(Box::new(
            TypeExpr::String,
        )))),
        Some(Value::Concrete(ConcreteValue::Map(map_value))),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(registry_prod); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let result = validate_export_param_ref_types_with_bindings(&exports, &bindings);
    assert!(
        result.is_ok(),
        "map(String) = {{ prod = registry_prod.account_id (String) }} should pass, got: {:?}",
        result
    );
}

#[test]
fn validate_export_param_ref_types_map_rejects_type_mismatch() {
    use crate::parser::InferredExportParam;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "organizations.account",
            vec![("account_id", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let registry_prod = Resource::with_provider("awscc", "organizations.account", "prod", None)
        .with_binding("registry_prod")
        .with_attribute(
            "account_id",
            Value::Concrete(ConcreteValue::String("111".into())),
        );

    let mut map_value = IndexMap::new();
    map_value.insert(
        "prod".to_string(),
        Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        ),
    );

    let exports = vec![InferredExportParam::for_test(
        "accounts",
        // declared as map(bool) — values should be rejected as they are strings
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Map(Box::new(
            TypeExpr::Bool,
        )))),
        Some(Value::Concrete(ConcreteValue::Map(map_value))),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(registry_prod); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let result = validate_export_param_ref_types_with_bindings(&exports, &bindings);
    assert!(
        result.is_err(),
        "map(Bool) = {{ prod = registry_prod.account_id }} (String) should be flagged as type mismatch"
    );
    let err = result
        .unwrap_err()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        err.contains("accounts") && err.contains("type mismatch"),
        "error should mention the export name and type mismatch, got: {err}"
    );
}

#[test]
fn export_param_ref_type_mismatch_displays_resolved_nested_ref_names() {
    use crate::parser::InferredExportParam;

    let source_schema = PatternResourceSchema::new("source.Item")
        .attribute(
            PatternAttributeSchema::optional(
                "values",
                PatternAttributeType::list(PatternAttributeType::ref_("S".to_string())),
            )
            .build(),
        )
        .with_def("S", PatternAttributeType::string());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        source_schema
            .validate()
            .expect("schema fixture must be valid"),
    );
    let source = Resource::with_provider("test", "source.Item", "source", None)
        .with_binding("source")
        .with_attribute("values", Value::Concrete(ConcreteValue::List(Vec::new())));
    let exports = vec![InferredExportParam::for_test(
        "values",
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Bool)),
        Some(Value::resource_ref(
            "source".to_string(),
            "values".to_string(),
            vec![],
        )),
    )];
    let mut parsed = empty_parsed();
    parsed.resources.push(source); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let error = validate_export_param_ref_types_with_bindings(&exports, &bindings)
        .expect_err("List<String> must not satisfy Bool")
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        error.contains("expected Bool, got List<String>"),
        "nested Ref names should be resolved in export errors: {error}",
    );
    assert!(
        !error.contains("Ref("),
        "schema-internal Ref names must not leak into the export error: {error}",
    );
}

#[test]
fn export_param_ref_types_flags_unknown_attribute() {
    use crate::parser::InferredExportParam;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema("ec2.Vpc", vec![("vpc_id", PatternAttributeType::string())]))
            .validate()
            .expect("schema fixture must be valid"),
    );

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "vpc_id",
            Value::Concrete(ConcreteValue::String("vpc-123".into())),
        );

    let exports = vec![InferredExportParam::for_test(
        "x",
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::String)),
        Some(Value::resource_ref(
            "vpc".to_string(),
            "bad_attr".to_string(),
            vec![],
        )),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let err = validate_export_param_ref_types_with_bindings(&exports, &bindings)
        .unwrap_err()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        err.contains("export 'x': unknown attribute 'bad_attr' on 'vpc' in reference vpc.bad_attr"),
        "expected export unknown attribute error, got: {err}",
    );
}

#[test]
fn export_param_ref_types_check_composition_attribute_membership_only() {
    use crate::parser::InferredExportParam;

    let exports = vec![
        InferredExportParam::for_test(
            "valid",
            crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Bool)),
            Some(Value::resource_ref(
                "instance".to_string(),
                "target_group_arn".to_string(),
                vec!["runtime_field".to_string()],
            )),
        ),
        InferredExportParam::for_test(
            "invalid",
            crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::Bool)),
            Some(Value::resource_ref(
                "instance".to_string(),
                "target_group_ar".to_string(),
                vec![],
            )),
        ),
    ];
    let mut parsed = empty_parsed();
    parsed
        .compositions
        .push(make_composition("instance", &["target_group_arn"]));
    let schemas = SchemaRegistry::new();
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let err = validate_export_param_ref_types_with_bindings(&exports, &bindings)
        .unwrap_err()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        err.lines().count(),
        1,
        "declared composition attr should skip type compatibility and narrowing: {err}",
    );
    assert!(
        err.contains("export 'invalid': unknown attribute 'target_group_ar' on 'instance'"),
        "expected composition membership error, got: {err}",
    );
}

#[test]
fn unchecked_export_checks_existence_and_attached_failure_skips_duplicate_walk() {
    use crate::parser::InferredExportParam;
    use crate::static_type::{
        ExportStaticType, InferenceError, InferredUncheckedReason, NotInferableReason, StaticType,
    };

    let schema_resource =
        Resource::with_provider("awscc", "elbv2.TargetGroup", "tg", None).with_binding("tg");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "elbv2.TargetGroup",
            vec![("target_group_arn", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    let exports = vec![
        InferredExportParam::for_test(
            "composition_bad",
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::NonInferableBinding {
                    binding: "instance".to_string(),
                }),
            )),
            Some(Value::resource_ref(
                "instance".to_string(),
                "target_group_ar".to_string(),
                vec![],
            )),
        ),
        InferredExportParam::for_test(
            "schema_bad",
            ExportStaticType::Inferred(StaticType::Unchecked(
                InferredUncheckedReason::NotInferable(NotInferableReason::NonInferableBinding {
                    binding: "tg".to_string(),
                }),
            )),
            Some(Value::resource_ref(
                "tg".to_string(),
                "target_group_ar".to_string(),
                vec![],
            )),
        ),
    ];
    let mut parsed = empty_parsed();
    parsed.resources.push(schema_resource); // allow: direct — fixture test inspection
    parsed
        .compositions
        .push(make_composition("instance", &["target_group_arn"]));
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );

    let all_errors = validate_export_param_ref_types_with_bindings(&exports, &bindings)
        .unwrap_err()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        all_errors.lines().count(),
        2,
        "an unreported Unknown export must check Schema and LiveComposition targets: {all_errors}",
    );
    assert!(all_errors.contains("export 'composition_bad': unknown attribute"));
    assert!(all_errors.contains("export 'schema_bad': unknown attribute"));

    let mut exports_with_failure = exports;
    exports_with_failure[1].static_type = ExportStaticType::Inferred(StaticType::Unchecked(
        InferredUncheckedReason::InferenceFailed(InferenceError::UnknownType {
            reason: "already reported".to_string(),
        }),
    ));
    let err = validate_export_param_ref_types_with_bindings(&exports_with_failure, &bindings)
        .unwrap_err()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        err.lines().count(),
        1,
        "only the exact index already reported by inference should be suppressed: {err}",
    );
    assert!(
        err.contains("export 'composition_bad': unknown attribute 'target_group_ar' on 'instance'"),
        "expected only the composition-backed miss, got: {err}",
    );
    assert!(
        !err.contains("schema_bad"),
        "the inference-reported index must not be walked again: {err}",
    );
}

#[test]
fn export_param_ref_types_flags_unknown_attribute_through_wait_binding() {
    use crate::parser::InferredExportParam;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![
                ("certificate_arn", PatternAttributeType::string()),
                ("status", PatternAttributeType::string()),
            ],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let cert = Resource::with_provider("aws", "acm.Certificate", "cert", None)
        .with_binding("cert")
        .with_attribute(
            "status",
            Value::Concrete(ConcreteValue::String("pending".into())),
        );

    let exports = vec![InferredExportParam::for_test(
        "x",
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::String)),
        Some(Value::resource_ref(
            "cert_issued".to_string(),
            "bad_attr".to_string(),
            vec![],
        )),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(cert); // allow: direct — fixture test inspection
    parsed.wait_bindings.push(WaitBinding {
        binding: RelativeBindingName::from("cert_issued"),
        target: RelativeBindingName::from("cert"),
        until_raw: "cert.status == 'issued'".to_string(),
        until_predicate: UntilPredicateAst {
            root: "cert".into(),
            attribute_segments: vec!["status".to_string()],
            rhs: RelativeRuntimeValue::Concrete(crate::resource::ConcreteValue::String(
                "issued".into(),
            )),
        },
        timeout_secs: None,
        depends_on: Default::default(),
        line: 1,
        declaration: Default::default(),
    });

    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let err = validate_export_param_ref_types_with_bindings(&exports, &bindings).unwrap_err();
    assert!(
        err.iter().any(|error| error.to_string().contains(
            "export 'x': unknown attribute 'bad_attr' on 'cert_issued' in reference cert_issued.bad_attr"
        )),
        "expected export wait binding unknown attribute error, got: {err:?}",
    );
}

#[test]
fn export_param_ref_types_flags_unknown_struct_field_inside_path() {
    use crate::parser::InferredExportParam;

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.Vpc",
            vec![(
                "config",
                PatternAttributeType::struct_(
                    "VpcConfig".to_string(),
                    vec![
                        PatternStructField::optional("sub_attr", PatternAttributeType::string())
                            .build(),
                    ],
                ),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None)
        .with_binding("vpc")
        .with_attribute(
            "config",
            Value::Concrete(ConcreteValue::Map(IndexMap::new())),
        );

    let exports = vec![InferredExportParam::for_test(
        "x",
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::String)),
        Some(Value::resource_ref(
            "vpc".to_string(),
            "config".to_string(),
            vec!["bad_field".to_string()],
        )),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let err = validate_export_param_ref_types_with_bindings(&exports, &bindings)
        .unwrap_err()
        .into_iter()
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        err.contains(
            "export 'x': unknown field 'bad_field' on struct 'VpcConfig' in reference vpc.config.bad_field"
        ),
        "expected export unknown struct field error, got: {err}",
    );
}

#[test]
fn validate_export_param_ref_types_skips_attached_inference_failure() {
    use crate::parser::InferredExportParam;
    use crate::static_type::{
        ExportStaticType, InferenceError, InferredUncheckedReason, StaticType,
    };

    let schema_resource =
        Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None).with_binding("vpc");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema("ec2.Vpc", vec![("vpc_id", PatternAttributeType::string())]))
            .validate()
            .expect("schema fixture must be valid"),
    );
    let exports = vec![InferredExportParam::for_test(
        "zone_id",
        ExportStaticType::Inferred(StaticType::Unchecked(
            InferredUncheckedReason::InferenceFailed(InferenceError::UnknownType {
                reason: "already reported".to_string(),
            }),
        )),
        Some(Value::resource_ref(
            "vpc".to_string(),
            "bad_attr".to_string(),
            vec![],
        )),
    )];

    let mut parsed = empty_parsed();
    parsed.resources.push(schema_resource); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let result = validate_export_param_ref_types_with_bindings(&exports, &bindings);
    assert!(
        result.is_ok(),
        "the exact unknown argument already reported by inference must be skipped, got {:?}",
        result
    );
}

#[test]
fn validate_export_param_ref_types_against_inferred_inputs() {
    use crate::parser::{InferredExportParam, UpstreamState};

    // Smoke test: a happy-path post-inference shape (bare TypeExpr,
    // matching attribute type) typechecks cleanly through the new
    // signature.
    let registry_prod = Resource::with_provider("awscc", "organizations.account", "prod", None)
        .with_binding("registry_prod")
        .with_attribute(
            "account_id",
            Value::Concrete(ConcreteValue::String("111".into())),
        );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "organizations.account",
            vec![("account_id", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let exports = vec![InferredExportParam::for_test(
        "id",
        crate::static_type::ExportStaticType::Declared(local_type(TypeExpr::String)),
        Some(Value::resource_ref(
            "registry_prod".to_string(),
            "account_id".to_string(),
            vec![],
        )),
    )];

    let _: &[UpstreamState] = &[]; // signature no longer takes upstream_states; doc only.
    let mut parsed = empty_parsed();
    parsed.resources.push(registry_prod); // allow: direct — fixture test inspection
    let bindings = crate::binding_index::BindingIndex::from_parsed(
        &parsed,
        &schemas,
        &crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let result = validate_export_param_ref_types_with_bindings(&exports, &bindings);
    assert!(result.is_ok(), "got {:?}", result);
}

/// Issue #2954 acceptance. A `Value::Deferred(DeferredValue::ResourceRef)` whose receiver is a
/// `List<String>` attribute (e.g. `resource_records = registry_dev.nameservers`
/// against `aws.route53.RecordSet.resource_records: List<String>`) must
/// not be rejected by `validate_resources`. Pre-Phase-2 the schema-level
/// `validate_list` returned `Type mismatch: expected List<String>, got
/// ResourceRef(...)`. Phase 2 (RFC #2972) makes the validate dispatcher
/// project `&Value` through `as_concrete()` so deferred values like
/// `ResourceRef` cannot reach `validate_list` by construction — the
/// type-fitness check for upstream-typed refs is the unified deferred-reference
/// validator's job (`BindingIndex::ref_type` / `validate_resource_ref_types`).
#[test]
fn validate_resources_accepts_resource_ref_in_list_position() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "route53.RecordSet",
            vec![(
                "resource_records",
                PatternAttributeType::list(PatternAttributeType::string()),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let record_set = Resource::with_provider("aws", "route53.RecordSet", "ns", None)
        .with_attribute(
            "resource_records",
            Value::resource_ref(
                "registry_dev".to_string(),
                "nameservers".to_string(),
                vec![],
            ),
        );

    let mut known = HashSet::new();
    known.insert("aws".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(record_set); // allow: direct — fixture test inspection
    let result = validate_resources(
        &parsed,
        &schemas,
        &known,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    );
    assert!(
        result.is_ok(),
        "ResourceRef in List<String> position must not produce a schema-level \
         type mismatch (#2954), got: {:?}",
        result
    );
}

/// Sibling of `..._in_list_position`: same invariant for a struct
/// **field** position. `collect_struct` previously skipped
/// `Value::Deferred(DeferredValue::ResourceRef)` per-field with an explicit `matches!` guard;
/// Phase 2 (RFC #2972) removed it because `collect_into` now projects
/// each field through `as_concrete()` and short-circuits on `None`.
/// This test pins the resulting behavior end-to-end.
#[test]
fn validate_resources_accepts_resource_ref_in_struct_field_position() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "test.StructHolder",
            vec![(
                "config",
                PatternAttributeType::struct_(
                    "Config".to_string(),
                    vec![
                        PatternStructField::optional("name", PatternAttributeType::string())
                            .build(),
                    ],
                ),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut config = IndexMap::new();
    config.insert(
        "name".to_string(),
        Value::resource_ref("vpc".to_string(), "name".to_string(), vec![]),
    );

    let holder = Resource::with_provider("aws", "test.StructHolder", "h", None)
        .with_attribute("config", Value::Concrete(ConcreteValue::Map(config)));

    let mut known = HashSet::new();
    known.insert("aws".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(holder); // allow: direct — fixture test inspection
    let result = validate_resources(
        &parsed,
        &schemas,
        &known,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    );
    assert!(
        result.is_ok(),
        "ResourceRef in Struct field position must not produce a schema-level type \
         mismatch (#2954), got: {:?}",
        result
    );
}

/// Sibling of `..._in_list_position`: same invariant for `Map<K,V>`.
#[test]
fn validate_resources_accepts_resource_ref_in_map_position() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "test.MapHolder",
            vec![(
                "tags",
                PatternAttributeType::map_with_key(
                    PatternAttributeType::string(),
                    PatternAttributeType::string(),
                ),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let holder = Resource::with_provider("aws", "test.MapHolder", "h", None).with_attribute(
        "tags",
        Value::resource_ref("orgs".to_string(), "tag_map".to_string(), vec![]),
    );

    let mut known = HashSet::new();
    known.insert("aws".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(holder); // allow: direct — fixture test inspection
    let result = validate_resources(
        &parsed,
        &schemas,
        &known,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    );
    assert!(
        result.is_ok(),
        "ResourceRef in Map position must not produce a schema-level type mismatch \
         (#2954), got: {:?}",
        result
    );
}

#[test]
fn validate_resources_checks_secret_literals_without_revealing_them() {
    let plaintext = "validate-time-plaintext";
    let schema = make_schema(
        "test.SecretHolder",
        vec![(
            "password",
            PatternAttributeType::refined_string(None, None, Some((Some(2), Some(4))), None),
        )],
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        schema.validate().expect("schema fixture must be valid"),
    );
    let parsed = crate::parser::parse_and_resolve(&format!(
        "let holder = test.test.SecretHolder {{\n  password = secret('{plaintext}')\n}}"
    ))
    .unwrap();
    let known = HashSet::from(["test".to_string()]);

    let error = validate_resources(
        &parsed,
        &schemas,
        &known,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    )
    .expect_err("the known value inside secret() must be length-checked");

    assert!(error.contains("(secret)"), "error: {error}");
    assert!(
        !error.contains(plaintext),
        "secret leaked in error: {error}"
    );
}

/// `validate_resources` must reject a resource whose schema declares an
/// `ExactlyOne` field group that is not satisfied — mirrors
/// `awscc.ec2.Vpc {}` with no cidr_block / ipam pool.
#[test]
fn validate_resources_rejects_missing_exactly_one_field_group() {
    let schema = make_schema(
        "ec2.Vpc",
        vec![
            ("cidr_block", PatternAttributeType::string()),
            ("ipv4_ipam_pool_id", PatternAttributeType::string()),
        ],
    )
    .field_group(
        crate::schema::FieldGroup::exactly_one(["cidr_block", "ipv4_ipam_pool_id"]).unwrap(),
    );

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        schema.validate().expect("schema fixture must be valid"),
    );

    let vpc = Resource::with_provider("awscc", "ec2.Vpc", "main-vpc", None);

    let mut known = HashSet::new();
    known.insert("awscc".to_string());

    let mut parsed = empty_parsed();
    parsed.resources.push(vpc); // allow: direct — fixture test inspection
    let err = validate_resources(
        &parsed,
        &schemas,
        &known,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        err.contains("exactly one of [cidr_block, ipv4_ipam_pool_id] must be set"),
        "expected field-group error, got: {err}"
    );
}

#[test]
fn enum_membership_violation_in_for_body_is_flagged() {
    // Regression for #2044: inside a `for` body, a string literal that
    // isn't a valid member of a Enum attribute must be flagged.
    let src = r#"
        provider test {
            source = 'x/y'
            version = '0.1'
            region = 'ap-northeast-1'
        }
        for _, id in orgs.xs {
            test.r.mode_holder {
                mode = "aaaa"
            }
        }
    "#;
    let parsed = crate::parser::parse(
        src,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (make_schema(
            "r.mode_holder",
            vec![(
                "mode",
                PatternAttributeType::enum_(
                    TypeIdentity::bare("Mode"),
                    Some(vec!["on".to_string(), "off".to_string()]),
                    vec![],
                    None,
                    None,
                ),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let mut known = HashSet::new();
    known.insert("test".to_string());

    let result = validate_resources(
        &parsed,
        &schemas,
        &known,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    );
    assert!(result.is_err(), "expected enum-mismatch error in for body");
    let err = result.unwrap_err();
    assert!(
        err.contains("aaaa"),
        "expected error to mention 'aaaa', got: {err}"
    );
}

/// Tests for #2094 — the four cases the PR 2 diagnostic must distinguish:
///   1. `mode = "aaa"` (string literal)   → StringLiteralExpectedEnum
///   2. `mode = aaa`   (bare invalid)     → InvalidEnumVariant (unchanged)
///   3. `mode = fast`  (bare valid)       → pass (unchanged)
///   4. fully-qualified form              → pass (unchanged)
fn mode_schema() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        (make_schema(
            "r.mode_holder",
            vec![(
                "mode",
                PatternAttributeType::enum_(
                    crate::schema::enum_identity("Mode", Some("test.r")),
                    Some(vec!["fast".to_string(), "slow".to_string()]),
                    vec![],
                    None,
                    None,
                ),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas
}

fn mode_known() -> HashSet<String> {
    let mut known = HashSet::new();
    known.insert("test".to_string());
    known
}

fn mode_context() -> LoadedProviderContext {
    let identity = crate::schema::enum_identity("Mode", Some("test.r"));
    context_with_accepting_custom_type_resolver(HashSet::from([
        crate::parser::OwnedTypeIdentity::provider(
            "test",
            identity,
            crate::schema::RefinedScalarKind::String,
        ),
    ]))
}

fn parse_mode(src: &str) -> ParsedFile {
    crate::parser::parse(
        src,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap()
}

#[test]
fn quoted_literal_enum_value_yields_string_literal_diagnostic() {
    // Case 1: `mode = "aaa"` — the parser tagged the path, validation
    // must surface the shape-mismatch message, not a variant list.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = "aaa"
        }
        "#,
    );
    let err = validate_resources(
        &parsed,
        &mode_schema(),
        &mode_known(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        err.contains("got a string literal"),
        "quoted literal must emit the shape-mismatch diagnostic, got: {err}"
    );
    assert!(
        err.contains("'aaa'"),
        "diagnostic must echo the user's literal, got: {err}"
    );
    assert!(
        err.contains("test.r.Mode.fast") || err.contains("test.r.Mode.slow"),
        "diagnostic must list fully-qualified valid variants, got: {err}"
    );
}

#[test]
fn bare_invalid_enum_value_keeps_invalid_variant_diagnostic() {
    // Case 2: `mode = aaa` — the parser did NOT tag this as a literal,
    // so the classic `InvalidEnumVariant` message must still be used.
    // Guards against regressing #2077/#2098 wording.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = aaa
        }
        "#,
    );
    let err = validate_resources(
        &parsed,
        &mode_schema(),
        &mode_known(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        !err.contains("got a string literal"),
        "bare identifier must NOT get the shape-mismatch diagnostic, got: {err}"
    );
    assert!(
        err.contains("aaa"),
        "diagnostic must still echo the typed value, got: {err}"
    );
}

#[test]
fn bare_valid_enum_value_passes() {
    // Case 3: `mode = fast` — bare identifier resolves to a real
    // variant and must pass cleanly.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = fast
        }
        "#,
    );
    let context = mode_context();
    assert!(
        validate_resources(
            &parsed,
            &mode_schema(),
            &mode_known(),
            &context,
            &mut Vec::new()
        )
        .is_ok(),
        "bare valid identifier must pass"
    );
}

#[test]
fn fully_qualified_enum_value_passes() {
    // Case 4: `mode = test.r.Mode.fast` — fully-qualified form must
    // still pass unchanged.
    let parsed = parse_mode(
        r#"
        let holder = test.r.mode_holder {
            mode = test.r.Mode.fast
        }
        "#,
    );
    let context = mode_context();
    assert!(
        validate_resources(
            &parsed,
            &mode_schema(),
            &mode_known(),
            &context,
            &mut Vec::new()
        )
        .is_ok(),
        "fully-qualified identifier must pass"
    );
}

/// `read` against a managed-only resource type emits a kind-specific error
/// pointing the user at the fix (drop the `read` keyword).
///
/// Companion to the inverse direction — non-`read` against a data-source-only
/// type — which the existing "is a data source and must be used with the
/// `read` keyword" tests already cover.
#[test]
fn read_against_managed_only_type_is_rejected() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "awscc",
        (make_schema(
            "ec2.Vpc",
            vec![("cidr_block", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        r#"
        let v = read awscc.ec2.Vpc {
            cidr_block = "10.0.0.0/16"
        }
        "#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let mut known = HashSet::new();
    known.insert("awscc".to_string());

    let err = validate_resources(
        &parsed,
        &schemas,
        &known,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        &mut Vec::new(),
    )
    .unwrap_err();
    assert!(
        err.contains("is a managed resource, not a data source"),
        "expected managed-only diagnostic, got: {err}"
    );
    assert!(
        err.contains("Remove the `read` keyword"),
        "expected fix hint, got: {err}"
    );
}

#[test]
fn rendered_typecheck_result_skips_value_unknown() {
    // RFC #2371 stage 3: a `Value::Deferred(DeferredValue::Unknown)` reaching this validator
    // (e.g. from a deferred-for body where a typed module-call argument
    // is bound to the loop var) must short-circuit rather than emit
    // `expected <type>, got unknown`.
    use crate::resource::{AuthoredApplyTimeUnknown as ApplyTimeUnknown, DeferredValue};
    use crate::validation::TypeExpr;

    let unknown = Value::Deferred(DeferredValue::Unknown(
        ApplyTimeUnknown::UpstreamAllExports {
            binding: "upstream".into(),
        },
    ));
    let cfg = ProviderContext::new(crate::EnvironmentSnapshot::empty());

    assert!(rendered_typecheck_result(&TypeExpr::String, &unknown, &cfg).is_none());
    assert!(rendered_typecheck_result(&TypeExpr::Int, &unknown, &cfg).is_none());
    assert!(rendered_typecheck_result(&TypeExpr::Bool, &unknown, &cfg).is_none());

    let struct_ty = TypeExpr::Struct {
        fields: vec![("name".to_string(), TypeExpr::String)],
    };
    assert!(rendered_typecheck_result(&struct_ty, &unknown, &cfg).is_none());

    let upstream = Value::Deferred(DeferredValue::Unknown(ApplyTimeUnknown::UpstreamExport {
        path: crate::resource::AccessPath::with_fields("net", "vpc", vec!["vpc_id".into()]),
    }));
    assert!(rendered_typecheck_result(&TypeExpr::String, &upstream, &cfg).is_none());
    assert!(rendered_typecheck_result(&struct_ty, &upstream, &cfg).is_none());
}

/// carina#3028: when a `ResourceRef` chains `[idx]` followed by
/// `.field`, the type checker must narrow the schema type through
/// each subscript (`List<T> → T`, `Map<_,V> → V`) before walking the
/// following field segments. Pre-fix the checker compared the whole
/// `List<Struct>` against the receiver `String`, ignoring the path
/// entirely past `binding.attribute`.
#[test]
fn ref_with_chained_subscript_then_field_narrows_through_list() {
    let mut schemas = SchemaRegistry::new();
    // `aws.acm.Certificate` exposes `domain_validation_options:
    // List<Struct{resource_record_name: String, ...}>`.
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![(
                "domain_validation_options",
                PatternAttributeType::list(PatternAttributeType::struct_(
                    "DomainValidationOption".to_string(),
                    vec![
                        PatternStructField::optional(
                            "resource_record_name",
                            PatternAttributeType::string(),
                        )
                        .build(),
                        PatternStructField::optional(
                            "resource_record_value",
                            PatternAttributeType::string(),
                        )
                        .build(),
                    ],
                )),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    // `aws.route53.RecordSet.name` is a plain String.
    schemas.insert(
        "aws",
        (make_schema(
            "route53.RecordSet",
            vec![("name", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let cert = aws.acm.Certificate {}\nlet validation = aws.route53.RecordSet { name = cert.domain_validation_options[0].resource_record_name }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let result = validate_resource_ref_types_for_test(&parsed, &schemas);
    assert!(
        result.is_ok(),
        "chained subscript-then-field should narrow List<Struct> → Struct → String, got: {:?}",
        result.err(),
    );
}

/// Sibling check for carina#3028: when the narrowed leaf is itself
/// incompatible with the receiver, the checker must still flag it —
/// narrowing is "honest" about the path, not a silent acceptance.
#[test]
fn ref_with_chained_subscript_then_field_rejects_real_mismatch() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![(
                "domain_validation_options",
                PatternAttributeType::list(PatternAttributeType::struct_(
                    "DomainValidationOption".to_string(),
                    vec![
                        PatternStructField::optional("rotation_count", PatternAttributeType::int())
                            .build(),
                    ],
                )),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    // Receiver `name` is String — the chained access yields Int.
    schemas.insert(
        "aws",
        (make_schema(
            "route53.RecordSet",
            vec![("name", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let cert = aws.acm.Certificate {}\nlet validation = aws.route53.RecordSet { name = cert.domain_validation_options[0].rotation_count }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let err = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(
        err.contains("expected String"),
        "expected real Int→String mismatch to be flagged, got: {err}"
    );
}

/// carina#3041: when a chained `[idx].field` references a struct
/// field that does not exist (typically because the user is still on
/// an older flat shape like `domain_validation_options[0].resource_record_name`
/// while the schema has migrated to a nested
/// `domain_validation_options[0].resource_record.name`), validation
/// must flag the unknown field by name with a suggestion. Pre-fix
/// the old per-consumer narrowing returned `None` silently, the caller
/// swallowed it, and the failure only surfaced at apply time with a
/// misleading "Add a `wait` block" message — `wait` could never have
/// helped, because the attribute will never exist under that spelling.
#[test]
fn ref_with_chained_field_on_struct_flags_unknown_field() {
    let mut schemas = SchemaRegistry::new();
    // Mirror the real `aws.acm.Certificate` shape after aws#295:
    // `domain_validation_options[*].resource_record: Struct{name, value}`.
    // Note: NO flat `resource_record_name` / `resource_record_value`
    // fields on the inner struct — those were removed by the nested
    // migration.
    schemas.insert(
        "aws",
        (make_schema(
            "acm.Certificate",
            vec![(
                "domain_validation_options",
                PatternAttributeType::list(PatternAttributeType::struct_(
                    "DomainValidation".to_string(),
                    vec![
                        PatternStructField::optional("domain_name", PatternAttributeType::string())
                            .build(),
                        PatternStructField::optional(
                            "resource_record",
                            PatternAttributeType::struct_(
                                "ResourceRecord".to_string(),
                                vec![
                                    PatternStructField::optional(
                                        "name",
                                        PatternAttributeType::string(),
                                    )
                                    .build(),
                                    PatternStructField::optional(
                                        "value",
                                        PatternAttributeType::string(),
                                    )
                                    .build(),
                                ],
                            ),
                        )
                        .build(),
                    ],
                )),
            )],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );
    schemas.insert(
        "aws",
        (make_schema(
            "route53.RecordSet",
            vec![("name", PatternAttributeType::string())],
        ))
        .validate()
        .expect("schema fixture must be valid"),
    );

    let parsed = crate::parser::parse(
        "let cert = aws.acm.Certificate {}\nlet validation = aws.route53.RecordSet { name = cert.domain_validation_options[0].resource_record_name }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).expect("reference fixture must parse");

    let err = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(
        err.contains("resource_record_name"),
        "error must name the unknown field, got: {err}",
    );
    // Struct name must appear (not a fuzzy "the word 'struct'" match —
    // the diagnostic commits to naming the enclosing struct so the
    // user knows where to look).
    assert!(
        err.contains("DomainValidation"),
        "error must identify the enclosing struct, got: {err}",
    );
    // Sibling fields must be enumerated so the user can discover the
    // new spelling (`resource_record.name`) even when `suggest_similar_name`'s
    // Levenshtein threshold (3 for a 20-char identifier) doesn't fire
    // on this specific typo distance. The `known fields:` prefix pins
    // the list, not the path — the path also contains `resource_record`
    // and would otherwise produce a trivially-satisfied assertion.
    assert!(
        err.contains("known fields:"),
        "error must enumerate known fields, got: {err}"
    );
    let known_section = err.split("known fields:").nth(1).unwrap_or("");
    assert!(
        known_section.contains("resource_record"),
        "known fields list must include the renamed field, got: {err}",
    );
    assert!(
        known_section.contains("domain_name"),
        "known fields list must enumerate all siblings, got: {err}",
    );
}

type ValidateConfigCallLog = std::sync::Arc<std::sync::Mutex<Vec<IndexMap<String, RuntimeValue>>>>;

/// Test-only factory that records every call to `validate_config` so a
/// test can assert what attributes did or didn't cross the WASM-equivalent
/// boundary. carina#3182.
struct RecordingFactory {
    name: String,
    seen: ValidateConfigCallLog,
}

impl RecordingFactory {
    fn new(name: &str) -> (Self, ValidateConfigCallLog) {
        let seen: ValidateConfigCallLog = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let f = Self {
            name: name.to_string(),
            seen: std::sync::Arc::clone(&seen),
        };
        (f, seen)
    }
}

impl crate::provider::ProviderFactory for RecordingFactory {
    fn name(&self) -> &str {
        &self.name
    }

    fn display_name(&self) -> &str {
        &self.name
    }

    fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
        Default::default()
    }

    fn validate_config(
        &self,
        attributes: &crate::provider::CanonicalProviderConfigAttributes,
        _session: &crate::provider::ProviderValidationInputs,
    ) -> crate::provider::ProviderResult<()> {
        self.seen
            .lock()
            .unwrap()
            .push(attributes.attributes().clone());
        Ok(())
    }

    fn create_provider(
        &self,
        _config: &crate::provider::ProviderSession,
        _initialize_outcome: crate::provider::InitializeOutcomeMint,
    ) -> futures::future::BoxFuture<
        '_,
        crate::provider::ProviderResult<crate::provider::ProviderInitialization>,
    > {
        unimplemented!("RecordingFactory::create_provider is not used in these tests")
    }

    fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
        crate::schema::ValidatedProviderSchemaWitness::new(
            self.schemas(),
            self.provider_config_attribute_types(),
            Default::default(),
            Vec::new(),
        )
        .unwrap()
    }

    fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
        Vec::new()
    }
}

#[test]
fn validate_provider_config_skips_attributes_with_deferred_refs() {
    // carina#3182: an attribute carrying an unresolved ref (e.g.
    // `assume_role = { role_arn = upstream.arn }` before
    // `load_upstream_states` has resolved it) must be dropped before
    // the plugin-side `validate_config` runs, so the WASM serializer
    // never sees the deferred value. Concrete sibling attributes
    // (`region`) must still reach the plugin.
    use crate::parser::ProviderConfig;
    use crate::resource::{AccessPath, DeferredValue};

    let role_arn = Value::Deferred(DeferredValue::ResourceRef {
        path: AccessPath::new("mgmt", "role_arn"),
    });
    let mut assume_inner: IndexMap<String, Value> = IndexMap::new();
    assume_inner.insert("role_arn".to_string(), role_arn);
    assume_inner.insert(
        "session_name".to_string(),
        Value::Concrete(ConcreteValue::String("sess".into())),
    );

    let mut attrs: IndexMap<String, Value> = IndexMap::new();
    attrs.insert(
        "region".to_string(),
        Value::Concrete(ConcreteValue::String("ap-northeast-1".into())),
    );
    attrs.insert(
        "assume_role".to_string(),
        Value::Concrete(ConcreteValue::Map(assume_inner)),
    );

    let pc = ProviderConfig {
        name: "aws".to_string(),
        attributes: attrs,
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    };
    let mut parsed = empty_parsed();
    parsed.providers.push(pc);

    let (factory, seen_handle) = RecordingFactory::new("aws");
    let factories: Vec<Box<dyn crate::provider::ProviderFactory>> = vec![Box::new(factory)];

    validate_provider_config(&parsed, &factories).expect("validate must not error");

    let seen = seen_handle
        .lock()
        .unwrap()
        .last()
        .cloned()
        .expect("validate_config must have been called once");
    assert!(
        seen.contains_key("region"),
        "literal attribute must reach validate_config; got: {seen:?}",
    );
    assert!(
        !seen.contains_key("assume_role"),
        "attribute containing a deferred ref must be filtered out; got: {seen:?}",
    );
}

#[test]
fn validate_provider_config_canonicalizes_enum_before_plugin_call() {
    use crate::parser::ProviderConfig;

    struct EnumRecordingFactory {
        seen: ValidateConfigCallLog,
    }

    impl crate::provider::ProviderFactory for EnumRecordingFactory {
        fn name(&self) -> &str {
            "awscc"
        }

        fn display_name(&self) -> &str {
            "awscc"
        }

        fn provider_config_attribute_types(&self) -> crate::schema::ValidatedConfigSchema {
            crate::schema::ValidatedConfigSchema::validate(BTreeMap::from([(
                "region".to_string(),
                PatternAttributeType::enum_(
                    crate::schema::enum_identity("Region", Some("aws")),
                    Some(vec!["ap-northeast-1".to_string()]),
                    vec![("ap-northeast-1".to_string(), "ap_northeast_1".to_string())],
                    None,
                    None,
                ),
            )]))
            .expect("test provider config schema must be valid")
        }

        fn validate_config(
            &self,
            attributes: &crate::provider::CanonicalProviderConfigAttributes,
            _session: &crate::provider::ProviderValidationInputs,
        ) -> crate::provider::ProviderResult<()> {
            self.seen
                .lock()
                .unwrap()
                .push(attributes.attributes().clone());
            Ok(())
        }

        fn create_provider(
            &self,
            _config: &crate::provider::ProviderSession,
            _initialize_outcome: crate::provider::InitializeOutcomeMint,
        ) -> futures::future::BoxFuture<
            '_,
            crate::provider::ProviderResult<crate::provider::ProviderInitialization>,
        > {
            unimplemented!("EnumRecordingFactory::create_provider is not used in this test")
        }

        fn schema_witness(&self) -> crate::schema::ValidatedProviderSchemaWitness {
            crate::schema::ValidatedProviderSchemaWitness::new(
                self.schemas(),
                self.provider_config_attribute_types(),
                Default::default(),
                Vec::new(),
            )
            .unwrap()
        }

        fn schemas(&self) -> Vec<crate::schema::ValidatedResourceSchema> {
            Vec::new()
        }
    }

    let mut parsed = empty_parsed();
    parsed.providers.push(ProviderConfig {
        name: "awscc".to_string(),
        attributes: IndexMap::from([(
            "region".to_string(),
            Value::Concrete(ConcreteValue::enum_identifier(
                "awscc.Region.ap_northeast_1",
            )),
        )]),
        default_tags: IndexMap::new(),
        source: None,
        version: None,
        revision: None,
        unresolved_attributes: IndexMap::new(),
        binding: None,
    });
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let factories: Vec<Box<dyn crate::provider::ProviderFactory>> =
        vec![Box::new(EnumRecordingFactory {
            seen: std::sync::Arc::clone(&seen),
        })];

    validate_provider_config(&parsed, &factories).expect("enum config must validate");

    let calls = seen.lock().unwrap();
    assert!(matches!(
        calls.last().and_then(|attrs| attrs.get("region")),
        Some(RuntimeValue::Concrete(RuntimeConcreteValue::CanonicalEnum(value)))
            if value.api_value() == "ap-northeast-1"
    ));
}

#[test]
fn value_contains_unresolved_ref_detects_nested_resource_ref() {
    // carina#3182: a `ResourceRef` nested inside a Map (the
    // `assume_role = { role_arn = upstream.arn }` shape) must be
    // detected so the validate path drops that attribute from the
    // WASM `validate_config` payload.
    use crate::resource::AccessPath;

    let role_arn = RuntimeValue::Deferred(RuntimeDeferredValue::ResourceRef {
        path: AccessPath::root("mgmt", "role_arn"),
    });
    let mut inner: IndexMap<String, RuntimeValue> = IndexMap::new();
    inner.insert("role_arn".to_string(), role_arn);
    inner.insert(
        "session_name".to_string(),
        RuntimeValue::Concrete(RuntimeConcreteValue::String("sess".into())),
    );
    let assume_role = RuntimeValue::Concrete(RuntimeConcreteValue::Map(inner));

    assert!(
        !matches!(
            assume_role
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification()),
            Ok(crate::resource::knowledge::Classification::Known(_))
        ),
        "nested ResourceRef inside a Map must be detected",
    );
}

#[test]
fn value_contains_unresolved_ref_false_for_pure_literal() {
    let mut m: IndexMap<String, RuntimeValue> = IndexMap::new();
    m.insert(
        "region".to_string(),
        RuntimeValue::Concrete(RuntimeConcreteValue::String("ap-northeast-1".into())),
    );
    let v = RuntimeValue::Concrete(RuntimeConcreteValue::Map(m));

    assert!(
        matches!(
            v.knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification()),
            Ok(crate::resource::knowledge::Classification::Known(_))
        ),
        "fully-literal value must not be flagged",
    );
}

#[test]
fn value_contains_unresolved_ref_detects_list_element() {
    use crate::resource::AccessPath;

    let v = RuntimeValue::Concrete(RuntimeConcreteValue::List(vec![
        RuntimeValue::Concrete(RuntimeConcreteValue::String("ok".into())),
        RuntimeValue::Deferred(RuntimeDeferredValue::ResourceRef {
            path: AccessPath::root("x", "y"),
        }),
    ]));
    assert!(
        !matches!(
            v.knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification()),
            Ok(crate::resource::knowledge::Classification::Known(_))
        ),
        "ResourceRef inside a List element must be detected",
    );
}

#[test]
fn value_contains_unresolved_ref_unwraps_secret() {
    use crate::resource::AccessPath;

    let secret_with_ref = RuntimeValue::Deferred(RuntimeDeferredValue::Secret(
        Box::new(RuntimeValue::Deferred(RuntimeDeferredValue::ResourceRef {
            path: AccessPath::root("vault", "token"),
        }))
        .into(),
    ));
    assert!(
        matches!(
            secret_with_ref
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                .map(|proof| proof.into_classification()),
            Ok(crate::resource::knowledge::Classification::Pending(_))
        ),
        "Secret(ResourceRef) must be pending"
    );

    let secret_literal = RuntimeValue::Deferred(RuntimeDeferredValue::Secret(
        Box::new(RuntimeValue::Concrete(RuntimeConcreteValue::String(
            "plaintext".into(),
        )))
        .into(),
    ));
    assert!(
        matches!(
            secret_literal
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation()).map(|proof| proof.into_classification())
                .map_err(crate::resource::knowledge_serialization_error)
                .and_then(crate::resource::require_known_value)
                .map(
                    crate::resource::knowledge::Classification::<crate::resource::NoPlaceholder>::Known
                ),
            Ok(crate::resource::knowledge::Classification::Known(_))
        ),
        "Secret(literal) must not be flagged",
    );
}

// ---------------------------------------------------------------------------
// carina#3239: live module-boundary type resolution.
// These cover the core resolver; the end-to-end CLI / LSP wiring is
// exercised by `carina-cli/tests/validate_unknown_custom_type_e2e.rs`.
// ---------------------------------------------------------------------------

fn enriched_context_with_iam_policy_arn() -> LoadedProviderContext {
    context_with_iam_policy_arn_validator()
}

#[test]
fn resolve_file_type_exprs_rejects_unknown_in_arguments() {
    let mut parsed = crate::parser::parse_file(
        "arguments { bad_arg: TotallyMadeUpType }",
        std::path::Path::new("arguments.crn"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let findings = crate::validation::deferred_types::with_session_for_test(
        &enriched_context_with_iam_policy_arn(),
        |session| resolve_file_type_exprs(&mut parsed, session).unwrap(),
    );

    assert_eq!(findings.len(), 1, "expected one finding, got {findings:?}");
    let only = findings[0].to_string();
    assert!(
        only.contains("argument 'bad_arg'")
            && only.contains("unknown custom type")
            && only.contains("TotallyMadeUpType"),
        "diagnostic must name the offending arg and the offending PascalCase type; got: {only}"
    );
}

#[test]
fn file_type_resolution_parser_error_coverage_ignores_rendered_wording() {
    let source_file = std::path::Path::new("arguments.crn");
    let source = "arguments {\n  bad_arg: TotallyMadeUpType\n}\n";
    let mut parsed = crate::parser::parse_file(
        source,
        source_file,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("bootstrap parsing must retain the unresolved declaration");
    let enriched_context = enriched_context_with_iam_policy_arn();
    let strict_error = crate::parser::parse_file(source, source_file, &enriched_context)
        .expect_err("enriched parsing must emit a typed unresolved declaration");
    let crate::parser::ParseError::UnresolvedDeclarationType {
        token,
        line,
        message: _message,
    } = strict_error
    else {
        panic!("unexpected strict parser error: {strict_error:?}");
    };
    let parser_error = crate::parser::ParseError::UnresolvedDeclarationType {
        token,
        line,
        message: "wording drift must not change typed coverage".to_string(),
    };

    let bootstrap_report = crate::validation::deferred_types::with_session_for_test(
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
        |session| {
            resolve_file_type_exprs_report(
                &mut parsed,
                session,
                Some(TypeResolutionParserError::new(source_file, &parser_error)),
            )
            .unwrap()
        },
    );
    assert!(bootstrap_report.diagnostics().is_empty());
    assert!(!bootstrap_report.covered_parser_error());

    let report =
        crate::validation::deferred_types::with_session_for_test(&enriched_context, |session| {
            resolve_file_type_exprs_report(
                &mut parsed,
                session,
                Some(TypeResolutionParserError::new(source_file, &parser_error)),
            )
            .unwrap()
        });

    assert_eq!(report.diagnostics().len(), 1);
    assert!(report.covered_parser_error());
    let diagnostic = &report.diagnostics()[0];
    assert_eq!(diagnostic.location().file().path(), source_file);
    let span = diagnostic.location().span();
    assert_eq!(
        &source[span.start_byte()..span.end_byte()],
        "TotallyMadeUpType"
    );
    assert_eq!(
        diagnostic.to_string(),
        "arguments.crn:2:12: argument 'bad_arg': unknown custom type 'TotallyMadeUpType'",
    );
}

#[test]
fn resolve_file_type_exprs_accepts_registered_bare_identity() {
    let mut parsed = crate::parser::parse_file(
        "arguments { policy: IamPolicyArn }",
        std::path::Path::new("arguments.crn"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    let findings = crate::validation::deferred_types::with_session_for_test(
        &enriched_context_with_iam_policy_arn(),
        |session| resolve_file_type_exprs(&mut parsed, session).unwrap(),
    );
    assert!(
        findings.is_empty(),
        "registered bare custom type must pass the resolver; got: {findings:?}"
    );
    assert!(matches!(
        &parsed.arguments[0].static_type,
        TypeExpr::SchemaType { owned: _ }
    ));
}

#[test]
fn resolve_file_type_exprs_walks_into_list_map_union_struct() {
    let mut parsed = crate::parser::parse_file(
        "arguments { in_list: list(UnknownA) in_map: map(UnknownB) in_union: UnknownC | UnknownD in_struct: struct { nested: UnknownE } }",
        std::path::Path::new("arguments.crn"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).unwrap();

    let findings = crate::validation::deferred_types::with_session_for_test(
        &enriched_context_with_iam_policy_arn(),
        |session| resolve_file_type_exprs(&mut parsed, session).unwrap(),
    );

    let joined = findings
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for expected in ["UnknownA", "UnknownB", "UnknownC", "UnknownD", "UnknownE"] {
        assert!(
            joined.contains(expected),
            "resolver must descend into List/Map/Union/Struct and surface {expected}; got: {joined}"
        );
    }
}

#[test]
fn resolve_file_type_exprs_defers_unknown_bare_types_until_the_catalog_is_loaded() {
    let mut parsed = crate::parser::parse_file(
        "arguments { bad_arg: TotallyMadeUpType }",
        std::path::Path::new("arguments.crn"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();

    // Bootstrap parsing has no authoritative provider catalog, so the live
    // resolver must retain the provisional simple type without diagnosing it.
    let ctx = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    assert!(!ctx.custom_types_loaded());
    assert!(ctx.known_custom_types().is_empty());
    let findings = crate::validation::deferred_types::with_session_for_test(&ctx, |session| {
        resolve_file_type_exprs(&mut parsed, session).unwrap()
    });
    assert!(findings.is_empty());

    // Once schemas have loaded, the same declaration becomes an actionable
    // unknown custom type.
    let findings = crate::validation::deferred_types::with_session_for_test(
        &enriched_context_with_iam_policy_arn(),
        |session| resolve_file_type_exprs(&mut parsed, session).unwrap(),
    );
    assert_eq!(
        findings.len(),
        1,
        "the enriched catalog must reject the deferred unknown type"
    );
}

#[test]
fn resolve_type_expr_accepts_registered_resource_ref() {
    use crate::parser::{ResourceTypePath, TypeExpr};

    let mut ctx = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    ctx.resource_types
        .insert(("aws".to_string(), "vpc".to_string()));

    let resolved = resolve_type_expr(
        &TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "vpc")),
        &ctx,
    )
    .expect("registered resource type must resolve");

    assert_eq!(
        resolved,
        TypeExpr::Ref(ResourceTypePath::new("aws", "vpc")),
        "registered resource refs such as aws.vpc must keep resolving as Ref"
    );
}

#[test]
fn resolve_type_expr_keeps_schema_owner_separate_from_identity_namespace() {
    use crate::parser::{OwnedTypeIdentity, ResourceTypePath, TypeExpr};

    let identity = TypeIdentity::new(Some("aws"), ["ec2"], "VpcId");
    let owned =
        OwnedTypeIdentity::provider("awscc", identity, crate::schema::RefinedScalarKind::String);
    let ctx = context_with_known_custom_types(HashSet::from([owned.clone()]));

    let resolved = resolve_type_expr(
        &TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "ec2.VpcId")),
        &ctx,
    )
    .expect("the semantic identity's authored path must resolve");

    assert_eq!(
        resolved,
        TypeExpr::SchemaType {
            owned: owned.unbounded_witness_for_test()
        }
    );
    assert_eq!(resolved.to_string(), "aws.ec2.VpcId");
}

#[test]
fn resolve_type_expr_keeps_every_owner_of_the_same_semantic_identity() {
    use crate::parser::{OwnedTypeIdentity, ResourceTypePath, TypeExpr};

    let identity = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
    let ctx = context_with_known_custom_types(HashSet::from([
        OwnedTypeIdentity::provider(
            "aws",
            identity.clone(),
            crate::schema::RefinedScalarKind::String,
        ),
        OwnedTypeIdentity::provider("awscc", identity, crate::schema::RefinedScalarKind::String),
    ]));

    let resolved = resolve_type_expr(
        &TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "iam.Role.Arn")),
        &ctx,
    );

    let TypeExpr::SchemaType { owned } =
        resolved.expect("all owners of one semantic identity must resolve as one annotation")
    else {
        panic!("shared identity must resolve as SchemaType")
    };
    assert_eq!(owned.provider_names().collect::<Vec<_>>(), ["aws", "awscc"]);
}

#[test]
fn consumer_export_resolution_retains_a_multi_owner_identity() {
    use crate::parser::{OwnedTypeIdentity, TypeExpr};

    let identity = TypeIdentity::new(Some("aws"), ["iam", "Role"], "Arn");
    let ctx = context_with_known_custom_types(HashSet::from([
        OwnedTypeIdentity::provider(
            "aws",
            identity.clone(),
            crate::schema::RefinedScalarKind::String,
        ),
        OwnedTypeIdentity::provider("awscc", identity, crate::schema::RefinedScalarKind::String),
    ]));
    let parsed = crate::parser::parse(
        "exports {\n  roles: list(aws.iam.Role.Arn) = [\"arn:one\"]\n}\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("multi-file export fixture syntax must parse");

    let declared = parsed.export_params[0]
        .static_type
        .as_ref()
        .expect("typed export must retain its parser-phase declaration");
    let crate::static_type::StaticType::Known(resolved) =
        resolve_consumer_static_type(declared, &ctx)
    else {
        panic!("typed export must retain its known declaration")
    };
    assert!(
        matches!(resolved.type_expr(), TypeExpr::List(inner) if matches!(inner.as_ref(), TypeExpr::SchemaType { .. })),
        "a known multi-owner identity must remain known: {resolved:?}"
    );
}

#[test]
fn resolve_type_expr_promotes_a_bare_provider_type_to_an_owned_key() {
    use crate::parser::{OwnedTypeIdentity, TypeExpr};

    let owned = OwnedTypeIdentity::provider(
        "awscc",
        TypeIdentity::bare("VpcId"),
        crate::schema::RefinedScalarKind::String,
    );
    let ctx = context_with_known_custom_types(HashSet::from([owned.clone()]));

    let resolved = resolve_type_expr(
        &crate::parser::bare_type_for_test("vpc_id".to_string()),
        &ctx,
    )
    .expect("the unique bare provider identity must resolve");

    assert_eq!(
        resolved,
        TypeExpr::SchemaType {
            owned: owned.unbounded_witness_for_test()
        }
    );
    assert_eq!(resolved.to_string(), "VpcId");
}

#[test]
fn parameter_handle_classification_requires_a_resolved_type() {
    use crate::parser::{ResourceTypePath, TypeExpr};

    let unresolved = TypeExpr::DottedUnresolved(ResourceTypePath::new("aws", "ec2.Vpc"));
    assert!(
        resolve_parameter_type(
            &unresolved,
            &ProviderContext::new(crate::EnvironmentSnapshot::empty())
        )
        .is_err(),
        "a bootstrap context must preserve uncertainty instead of guessing value vs handle",
    );

    let mut enriched = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    enriched
        .resource_types
        .insert(("aws".to_string(), "ec2.Vpc".to_string()));
    assert_eq!(
        resolve_parameter_type(&unresolved, &enriched).unwrap(),
        ResolvedParameterType::Handle(ResourceTypePath::new("aws", "ec2.Vpc")),
    );
}

fn attribute_reference_message_fixture(module_directory: &str) -> CompositionAttributeRefError {
    CompositionAttributeRefError {
        error: AttributeParamRefError {
            attribute: "bad".to_string(),
            path: crate::resource::DiagnosticScope::Public.path(
                crate::resource::ReferencePath::Binding(RootScope::new().resolve("source".into())),
            ),
            kind: ModuleCallRefErrorKind::TypeMismatch {
                types: MismatchTypeNames::for_type_exprs(&TypeExpr::String, &TypeExpr::Int),
                declared: None,
            },
        },
        module_name: "shared".to_string(),
        module_source: Some("../shared".to_string()),
        module_directory: Some(crate::paths::ResolvedPath::new(module_directory).unwrap()),
        call_paths: Vec::new(),
        reference_position: 0,
    }
}

#[test]
fn module_call_type_resolution_message_exposes_its_typed_source() {
    let message = ModuleCallFindingMessage::type_resolution_for_test(
        "module declaration".to_string(),
        crate::static_type::StaticTypeResolutionError::UnknownCustomType {
            name: "Missing".to_string(),
        },
    );

    let source = std::error::Error::source(&message)
        .expect("type-resolution findings must expose their typed source");
    assert!(
        source.downcast_ref::<TypeResolutionDiagnostic>().is_some(),
        "type-resolution findings must retain TypeResolutionDiagnostic, not rendered text",
    );
}

#[test]
fn module_attribute_reference_message_exposes_its_typed_source() {
    let message = ModuleCallFindingMessage::attribute_reference_for_test(
        attribute_reference_message_fixture("/modules/one"),
    );

    let source = std::error::Error::source(&message)
        .expect("attribute-reference findings must expose their typed source");
    assert!(
        source
            .downcast_ref::<CompositionAttributeRefError>()
            .is_some(),
        "attribute-reference findings must retain CompositionAttributeRefError",
    );
}

#[test]
fn module_reference_coverage_uses_typed_call_identity_not_display_text() {
    let source = "let child = use { source = './child' }\nchild { value = 1 }\n";
    let first = crate::parser::parse_file(
        source,
        std::path::Path::new("first.crn"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("first fixture parse");
    let second = crate::parser::parse_file(
        source,
        std::path::Path::new("second.crn"),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .expect("second fixture parse");
    let kind = ModuleCallRefErrorKind::TypeMismatch {
        types: MismatchTypeNames::for_type_exprs(&TypeExpr::String, &TypeExpr::Int),
        declared: None,
    };
    let path = ReferencePath::Binding("source".into());
    let finding =
        ModuleCallFindingMessage::ReferenceType(Box::new(ModuleCallReferenceDiagnostic {
            call_label: "child".to_string(),
            identity: ModuleCallReferenceIdentity {
                call: first.module_calls[0].id().clone(),
                argument: "value".to_string(),
                path: ModuleCallDiagnosticPath::Authored(path.clone()),
                reference_position: Some(0),
                binding_use: None,
                kind: kind.clone(),
            },
            display_path: ModuleCallDiagnosticPath::Authored(path.clone()),
            declared_constraint: false,
        }));
    let resolver_error = ModuleCallRefError {
        call_label: "child".to_string(),
        identity: ModuleCallReferenceIdentity {
            call: second.module_calls[0].id().clone(),
            argument: "value".to_string(),
            path: ModuleCallDiagnosticPath::Authored(path),
            reference_position: Some(0),
            binding_use: None,
            kind,
        },
        argument_span: second.module_calls[0].source_span(),
    };

    assert_eq!(
        finding.to_string(),
        resolver_error.to_string(),
        "the fixture must render identically despite distinct authored call identities",
    );
    assert!(
        !finding.covers_reference_type(&resolver_error),
        "coverage must compare ModuleCallId as part of the typed identity",
    );
}

#[test]
fn module_attribute_reference_dedup_uses_typed_identity_not_display_text() {
    let first = ModuleCallFindingMessage::attribute_reference_for_test(
        attribute_reference_message_fixture("/modules/one"),
    );
    let second = ModuleCallFindingMessage::attribute_reference_for_test(
        attribute_reference_message_fixture("/modules/two"),
    );

    assert_eq!(
        first.to_string(),
        second.to_string(),
        "the fixture must isolate identities that intentionally render alike",
    );
    assert_ne!(
        first, second,
        "deduplication must not merge distinct typed attribute-reference failures",
    );
}

#[test]
fn module_type_resolution_dedup_uses_typed_identity_not_display_text() {
    let diagnostic = |source_file| {
        ModuleCallFindingMessage::TypeResolution(Box::new(TypeResolutionDiagnostic::description(
            "module declaration".to_string(),
            crate::static_type::StaticTypeResolutionError::UnknownCustomType {
                name: "Missing".to_string(),
            }
            .into(),
            parser_test_location(std::path::Path::new(source_file)),
        )))
    };
    let first = diagnostic("first.crn");
    let second = diagnostic("second.crn");

    assert_eq!(
        first.to_string(),
        second.to_string(),
        "the fixture must isolate source identities that intentionally render alike",
    );
    assert_ne!(
        first, second,
        "deduplication must retain distinct typed type-resolution failures",
    );
}

#[cfg(unix)]
#[test]
fn composition_error_identity_survives_symlink_retarget() {
    let temp = tempfile::tempdir().unwrap();
    let original = temp.path().join("original");
    let replacement = temp.path().join("replacement");
    let alias = temp.path().join("alias");
    std::fs::create_dir(&original).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    std::os::unix::fs::symlink(&original, &alias).unwrap();
    let error = attribute_reference_message_fixture(alias.to_str().unwrap());
    let identity = error.module_identity();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&replacement, &alias).unwrap();
    assert_eq!(error.module_identity(), identity);
}

#[test]
fn deferred_loop_placeholder_access_checks_after_materialization() {
    use crate::resource::{AccessMissPolicy, LoopSlot, PathSegment, apply_postfix_access};
    let parsed = ParsedFile::default();
    let schemas = SchemaRegistry::new();
    let provider = crate::parser::LoadedProviderContext::empty_for_test();
    let index = BindingIndex::from_parsed(&parsed, &schemas, &provider);
    let inference = crate::parser::retained::value_dependent_judgement_for_test();
    let expected = crate::schema::CompiledAttributeType::string();
    let field = PathSegment::Field {
        name: "action".into(),
    };
    let pending = apply_postfix_access(
        Value::Deferred(DeferredValue::Placeholder(ParserPlaceholder::Loop(
            LoopSlot::Value,
        ))),
        field.clone(),
        1,
        AccessMissPolicy::Defer,
    )
    .unwrap();
    assert_eq!(
        check_deferred_access_type(
            &pending,
            crate::schema::TypeInSchema::schemaless(&expected).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER)
            ),
            &inference,
            &schemas,
            &index
        ),
        Ok(())
    );
    for (action, assignable) in [
        (Value::Concrete(ConcreteValue::String("part".into())), true),
        (Value::Concrete(ConcreteValue::Int(42)), false),
    ] {
        let materialized = apply_postfix_access(
            Value::Concrete(ConcreteValue::Map(IndexMap::from([(
                "action".into(),
                action,
            )]))),
            field.clone(),
            1,
            AccessMissPolicy::Defer,
        )
        .unwrap();
        let materialized = materialized.into_expanded().unwrap();
        let result = check_runtime_value_type(
            inference::RuntimeTypeInput::computed_for_test(&materialized),
            crate::schema::TypeInSchema::schemaless(&expected).with_authority(
                crate::schema::assignment_catalog_without_edges_for_test(),
                crate::schema::SinkAuthority::Owner(&TEST_OWNER),
            ),
            &inference::InferenceBindings::new(),
            &schemas,
            &index,
        );
        assert_eq!(
            result.is_ok(),
            assignable,
            "materialized values must be checked against the sink: {result:?}"
        );
    }
}

#[test]
fn file_type_resolution_report_mints_resolved_dependency_argument_types()
-> Result<(), crate::parser::AnalysisTreeError> {
    let context = ProviderContext {
        resource_types: HashSet::from([("mock".to_string(), "test.resource".to_string())]),
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                "aws",
                TypeIdentity::new(Some("aws"), ["iam", "Policy"], "Arn"),
                crate::schema::RefinedScalarKind::String,
            )]),
            Box::new(|_target, _value| Ok(())),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    let mut parsed = crate::parser::parse(
        "arguments { rec: aws.iam.Policy.Arn r: mock.test.resource }\nlet consumer = mock.test.resource { directives { depends_on = [rec, r] } }\n",
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    ).unwrap();
    assert!(matches!(
        parsed.arguments[0].static_type,
        TypeExpr::DottedUnresolved(_)
    ));
    let report = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        resolve_file_type_exprs_report(&mut parsed, session, None).unwrap()
    });
    assert!(
        report.diagnostics().is_empty(),
        "{:?}",
        report.diagnostics()
    );
    let diagnostics = depends_on::validate_depends_on(
        &crate::parser::AnalysisTree::try_from_file(&parsed)?,
        report.resolved_argument_types(),
    );
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0].severity, depends_on::Severity::Error);
    assert_eq!(
        diagnostics[0].message,
        "directives.depends_on: expected a managed resource handle; got aws.iam.Policy.Arn"
    );
    Ok(())
}

#[test]
fn file_type_resolution_failure_classifies_each_dependency_argument_independently()
-> Result<(), crate::parser::AnalysisTreeError> {
    let mut parsed = crate::parser::parse(
        r#"arguments {
  bad: aws.route53.Missing.Id
  value: aws.iam.Policy.Arn
  handle: mock.test.resource
}
let consumer = mock.test.resource {
  directives { depends_on = [bad, bad, value, handle, missing, consumer] }
}
"#,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap();
    let context = ProviderContext {
        resource_types: HashSet::from([("mock".to_string(), "test.resource".to_string())]),
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                "aws",
                TypeIdentity::new(Some("aws"), ["iam", "Policy"], "Arn"),
                crate::schema::RefinedScalarKind::String,
            )]),
            Box::new(|_target, _value| Ok(())),
        ),
        ..crate::parser::ProviderContext::new(crate::EnvironmentSnapshot::empty())
    };
    let report = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        resolve_file_type_exprs_report(&mut parsed, session, None).unwrap()
    });
    assert!(!report.diagnostics().is_empty());
    let argument_types = report.resolved_argument_types();
    let diagnostics = depends_on::validate_depends_on(
        &crate::parser::AnalysisTree::try_from_file(&parsed)?,
        argument_types,
    );
    for expected in [
        "got aws.iam.Policy.Arn",
        "binding 'missing' is not declared",
        "self-reference is not allowed",
        "binding 'bad' is listed multiple times",
    ] {
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains(expected)),
            "missing {expected:?}: {diagnostics:#?}"
        );
    }
    assert!(
        diagnostics.iter().all(|diagnostic| {
            diagnostic.severity != depends_on::Severity::Error
                || !matches!(
                    &diagnostic.kind,
                    depends_on::DependsOnDiagnosticKind::Element { dependency, .. } if matches!(dependency.as_str(), "bad" | "handle")
                )
        }),
        "only the unresolved argument's handle-kind check must be skipped: {diagnostics:#?}"
    );
    Ok(())
}

fn parser_test_location(path: &std::path::Path) -> crate::parser::DeclarationLocation {
    crate::parser::parse_file(
        "let anchor = 1",
        path,
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .parser_binding_metadata
    .syntax()
    .declarations()[0]
        .location()
        .clone()
}

fn missing_inventory_fixture(
    rhs: &str,
) -> (ParsedFile, crate::module_resolver::ResolvedModuleSignatures) {
    let provider = LoadedProviderContext::empty_for_test();
    let mut parsed = crate::parser::parse_and_resolve(&format!("let src = mock.test.Source {{}}\nlet n = \"${{src.name}}\"\nlet template = use {{ source = './child' }}\nlet child = template {{ x = {rhs} }}")).unwrap();
    *parsed.parser_binding_metadata.syntax.expressions_mut() =
        crate::parser::retained::Expressions::default();
    let signatures = HashMap::from([(
        "template".to_string(),
        crate::module_resolver::ResolvedModuleSignature {
            arguments: vec![crate::parser::ArgumentParameter {
                name: "x".to_string(),
                static_type: resolve_consumer_static_type_for_signature(
                    &TypeExpr::Int,
                    provider.as_provider_context(),
                ),
                default: None,
                description: None,
                validations: Vec::new(),
            }],
            attributes: IndexMap::new(),
            requires: Vec::new(),
        },
    )]);
    (parsed, signatures)
}

#[test]
fn module_argument_fallback_requires_authored_evidence() {
    let (parsed, signatures) = missing_inventory_fixture("\"${src.name}\"");
    let findings = module_call_findings_for_test(
        &parsed,
        &signatures,
        &LoadedProviderContext::empty_for_test(),
    );
    assert!(
        findings
            .iter()
            .any(|finding| finding.kind() == ModuleCallFindingKind::ValueType),
        "an authored argument used pending evidence: {findings:?}"
    );
}

#[test]
fn bare_typeexpr_sink_requires_authored_evidence() {
    let (parsed, signatures) = missing_inventory_fixture("n");
    let provider = LoadedProviderContext::empty_for_test();
    let schemas = SchemaRegistry::new();
    let loaded = crate::config_loader::load_buffer_configuration(
        &parsed,
        &schemas,
        provider.as_provider_context(),
    );
    let context = ValidationTypeContext::new(loaded.judged(), &provider, &signatures);
    let report = check_bare_binding_uses(&context);
    assert!(!report.is_ready(), "a TypeExpr sink used pending evidence");
}

#[test]
fn bare_binding_precheck_preserves_rejection_after_constant_folding() {
    let config = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    for rhs in ["'present' ?? 42", "['present', ('present' ?? 42)][0]"] {
        let parsed = crate::parser::parse_and_resolve(&format!(
            "let bad = {rhs}\nlet template = use {{ source = './child' }}\nlet child = template {{ x = bad }}"
        ))
        .unwrap();
        let loaded = crate::config_loader::load_buffer_configuration(&parsed, &schemas, &config);
        let signatures = crate::module_resolver::ResolvedModuleSignatures::default();
        let context =
            ValidationTypeContext::new_providerless(loaded.judged(), &config, &signatures);
        let record = parsed
            .bare_binding_uses()
            .records()
            .iter()
            .find(|record| record.authored.name == "bad")
            .unwrap();
        let crate::parser::BareBindingSource::Value {
            value,
            declared_type,
        } = &record.source
        else {
            panic!("value binding expected");
        };
        assert!(
            matches!(
                (value)
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification()),
                Ok(crate::resource::knowledge::Classification::Known(_))
            ),
            "{rhs}: {value:?}"
        );
        let expected = TypeExpr::String;
        let error = precheck_value_source(
            &record.authored,
            value,
            declared_type.as_deref(),
            RefSink::TypeExpr(&expected),
            &context,
            &|path| ModuleCallDiagnosticPath::Authored(path.to_owned()),
        )
        .expect("a folded value must not overturn rejected admission");
        assert!(
            matches!(error, BindingResolutionError::DeferredAccess(error)
            if matches!(*error, DeferredAccessTypeError::InvalidAccess {
                error: inference::InferenceError::IncompatibleCoalesceTypes { .. }
            })),
            "{rhs}"
        );
    }
}

fn expression_validation_config() -> ProviderContext {
    ProviderContext::new(crate::EnvironmentSnapshot::empty())
}

#[test]
fn review_r3_interpolation_reports_every_reference_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::optional(
                    "first",
                    PatternAttributeType::list(PatternAttributeType::string()),
                )
                .build(),
            )
            .attribute(
                PatternAttributeSchema::optional(
                    "second",
                    PatternAttributeType::list(PatternAttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Sink")
            .attribute(
                PatternAttributeSchema::optional("value", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse("let src = mock.test.Source {}\nlet sink = mock.test.Sink { value = \"${src.first}-${src.second}\" }\n", &expression_validation_config()).unwrap();
    let error = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(
        error.contains("src.first") && error.contains("src.second"),
        "{error}"
    );
}

#[test]
fn review_r5_nested_mismatch_uses_the_failing_reference_origin() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::optional("good", PatternAttributeType::string()).build(),
            )
            .attribute(PatternAttributeSchema::optional("bad", PatternAttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Sink")
            .attribute(
                PatternAttributeSchema::optional(
                    "values",
                    PatternAttributeType::list(PatternAttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse("let src = mock.test.Source {}\nlet sink = mock.test.Sink { values = [src.good, src.bad] }\n", &expression_validation_config()).unwrap();
    let error = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert!(error.contains("(from src.bad)"), "{error}");
    assert!(!error.contains("(from src.good)"), "{error}");
}

#[test]
fn review_r9_nested_symbolic_findings_are_rendered_once_per_path() {
    let catalog = LoadedProviderContext::empty_for_test();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(PatternAttributeSchema::optional("bad", PatternAttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Sink")
            .attribute(
                PatternAttributeSchema::optional(
                    "values",
                    PatternAttributeType::list(PatternAttributeType::string()),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse("let src = mock.test.Source {}\nlet sink = mock.test.Sink { values = [src.bad, src.bad] }\n", catalog.as_provider_context()).unwrap();
    let bindings = BindingIndex::from_parsed(&parsed, &schemas, catalog.as_provider_context());
    let loaded = crate::config_loader::load_buffer_configuration(
        &parsed,
        &schemas,
        catalog.as_provider_context(),
    );
    let error = validate_resource_admissions(
        loaded.judged(),
        &bindings,
        &HashSet::from(["mock".into()]),
        &mut Vec::new(),
    )
    .unwrap_err()
    .to_string();
    assert_eq!(error.lines().count(), 2, "{error}");
}

#[test]
fn admission_deferred_read_keeps_main_verdict_across_files() {
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/admission_deferred_read");
    let catalog = LoadedProviderContext::empty_for_test();
    let catalog = catalog.as_provider_context();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("iam.Roles")
            .as_data_source()
            .attribute(
                PatternAttributeSchema::required("name_regex", PatternAttributeType::string())
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    // Main leaves these parser-side read templates to the expansion seam.
    // Even a literal mismatch must not introduce a new rejection here.
    for replacement in ["n", "42"] {
        let directory = tempfile::tempdir().unwrap();
        for name in ["main.crn", "exports.crn", "providers.crn", "backend.crn"] {
            let source = std::fs::read_to_string(fixture.join(name)).unwrap();
            std::fs::write(
                directory.path().join(name),
                source.replace("name_regex = n", &format!("name_regex = {replacement}")),
            )
            .unwrap();
        }
        let path = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let parsed = crate::config_loader::parse_directory(&path, catalog)
            .unwrap()
            .into_parsed();
        let tree = crate::parser::AnalysisTree::try_from_file(&parsed).unwrap();
        assert_eq!(tree.resources().count(), 1);
        assert_eq!(tree.project_resources().count(), 0);
        let bindings = BindingIndex::from_parsed(&parsed, &schemas, catalog);
        let configuration =
            crate::config_loader::load_buffer_configuration(&parsed, &schemas, catalog);
        validate_resource_admissions(
            configuration.judged(),
            &bindings,
            &HashSet::from(["mock".into()]),
            &mut Vec::new(),
        )
        .unwrap();
    }
}

fn expression_validation_directory(
    source: &str,
    sibling: &str,
) -> (tempfile::TempDir, crate::paths::ResolvedPath) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("main.crn"), source).unwrap();
    std::fs::write(dir.path().join("source.crn"), sibling).unwrap();
    let path = crate::paths::ResolvedPath::new(dir.path()).unwrap();
    (dir, path)
}

fn expression_validation_directory_file(source: &str) -> ParsedFile {
    let (_dir, path) = expression_validation_directory(source, "let src = mock.test.Source {}\n");
    crate::config_loader::parse_directory(&path, &expression_validation_config())
        .unwrap()
        .into_parsed()
}

#[test]
fn function_argument_interpolation_is_checked_locally() {
    let result = crate::parser::parse_and_resolve(
        "fn f(n: Int): Int { n }\nlet src = mock.test.Source {}\nlet result = f(\"${src.name}-inline\")",
    );
    assert!(
        result.is_err(),
        "authored function argument bypassed the Int sink"
    );
}

#[test]
fn function_return_interpolation_is_checked_across_files() {
    let (_dir, path) = expression_validation_directory(
        "let src = mock.test.Source {}\nlet result = f(src.name)",
        "fn f(n: String): Int { \"${n}-inline\" }",
    );
    let result = crate::config_loader::parse_directory(&path, &expression_validation_config());
    assert!(
        result.is_err(),
        "sibling function return bypassed the Int sink"
    );
}

#[test]
fn function_return_interpolation_is_checked_locally() {
    let result = crate::parser::parse_and_resolve(
        "fn f(n: String): Int { \"${n}-inline\" }\nlet src = mock.test.Source {}\nlet result = f(src.name)",
    );
    assert!(
        result.is_err(),
        "authored function return bypassed the Int sink"
    );
}

#[test]
fn concrete_function_arguments_keep_literal_precision() {
    let result = crate::parser::parse_and_resolve(
        "fn f(n: 'dev' | 'prod'): String { n }\nlet result = f('prod')",
    );
    assert!(
        result.is_ok(),
        "a valid literal was widened before its constraint check: {result:?}"
    );
}

#[test]
fn folded_children_keep_every_failure() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Source")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "name",
                    crate::schema::PatternAttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    for source in [
        "let a = 'x'\nlet b = a ?? [1, 'y']",
        "let r = mock.test.Source {}\nlet a = 'x'\nlet b = a ?? r.no_such_attr",
        "let a = 'x'\nlet b = length([a ?? [1, 'y']])",
    ] {
        let parsed = crate::parser::parse_and_resolve(source).unwrap();
        let loaded = crate::config_loader::load_buffer_configuration(
            &parsed,
            &schemas,
            &expression_validation_config(),
        );
        let judged = loaded.judged();
        assert_eq!(
            judged.report().findings().len(),
            1,
            "{source}: {:?}",
            judged.report().findings()
        );
        assert!(matches!(
            judged
                .report()
                .binding(&RelativeBindingName::new("b"))
                .unwrap()
                .static_type(),
            Err(crate::static_type::InferenceIssue::Failed(_))
        ));
    }
}

#[test]
fn resource_sink_does_not_repeat_child_findings() {
    let parsed = expression_validation_directory_file(
        "let sink = mock.test.Source { name = \"${src.no_such_attr}\" }",
    );
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Source")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "name",
                    crate::schema::PatternAttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let loaded = crate::config_loader::load_buffer_configuration(
        &parsed,
        &schemas,
        &expression_validation_config(),
    );
    let report = loaded.judged().report();
    assert!(report.findings().is_empty(), "{:?}", report.findings());
    let sink = report.binding(&RelativeBindingName::new("sink")).unwrap();
    assert_eq!(
        sink.check_owner(),
        crate::parser::retained::ExpressionCheckOwner::BindingIndex
    );
    assert!(matches!(
        sink.static_type(),
        Err(crate::static_type::InferenceIssue::Failed(
            inference::InferenceError::UnknownAttribute { .. }
        ))
    ));
}

#[test]
fn deferred_loop_keys_and_indexes_are_not_concrete() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Source")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "tags",
                    crate::schema::PatternAttributeType::map(
                        crate::schema::PatternAttributeType::string(),
                    ),
                )
                .build(),
            )
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "names",
                    crate::schema::PatternAttributeType::list(
                        crate::schema::PatternAttributeType::string(),
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        crate::schema::PatternResourceSchema::new("test.Sink")
            .attribute(
                crate::schema::PatternAttributeSchema::optional(
                    "name",
                    crate::schema::PatternAttributeType::refined_string(
                        Some(crate::schema::TypeIdentity::new(
                            Some("mock"),
                            ["test", "Sink"],
                            "Name",
                        )),
                        None,
                        None,
                        None,
                    ),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    for pattern in ["for k, v in src.tags", "for (i, v) in src.names"] {
        let name = if pattern.contains("k,") { "k" } else { "i" };
        let parsed = crate::parser::parse_and_resolve(&format!("let src = mock.test.Source {{}}\n{pattern} {{ mock.test.Sink {{ name = \"${{{name}}}-inline\" }} }}")).unwrap();
        let loaded = crate::config_loader::load_buffer_configuration(
            &parsed,
            &schemas,
            &expression_validation_config(),
        );
        let judged = loaded.judged();
        assert!(
            judged.report().findings().is_empty(),
            "{pattern}: {:?}",
            judged.report().findings()
        );
        let tree = crate::parser::AnalysisTree::try_from_file(loaded.parsed()).unwrap();
        let resource = tree
            .resources()
            .find(|r| r.id().resource_type == "test.Sink")
            .unwrap();
        let expression = judged
            .report()
            .expression(resource.attribute_value_location("name", &crate::schema::FieldPath::new()))
            .unwrap();
        assert_eq!(
            crate::parser::retained::admit_expression(expression, &TypeExpr::String),
            Ok(crate::parser::retained::ExpressionAdmission::DeferredInterpolation),
            "{pattern}: {expression:?}"
        );
    }
}

#[test]
fn module_argument_interpolation_cannot_bypass_substitution() {
    let (dir, path) = expression_validation_directory(
        "let template = use { source = './child' }\nlet child = template { n = \"${src.name}-inline\" }",
        "let src = mock.test.Source {}",
    );
    std::fs::create_dir(dir.path().join("child")).unwrap();
    std::fs::write(
        dir.path().join("child/main.crn"),
        "arguments { n: Int }\nattributes { value = n }",
    )
    .unwrap();
    let parsed =
        crate::config_loader::parse_directory(&path, &expression_validation_config()).unwrap();
    let result =
        deferred_types::with_session_for_test(&expression_validation_config(), |session| {
            crate::module_resolver::resolve_modules_with_providerless_config(
                parsed,
                &path,
                session,
                &SchemaRegistry::new(),
            )
        });
    assert!(
        result.is_err(),
        "module argument interpolation bypassed substitution checks"
    );
}

#[test]
fn instances_retain_each_authored_location() {
    let (dir, path) = expression_validation_directory(
        "let template = use { source = './child' }\nlet a = template { value = 42 }\nlet b = template { value = 'text' }",
        "",
    );
    std::fs::create_dir(dir.path().join("child")).unwrap();
    std::fs::write(
        dir.path().join("child/main.crn"),
        "arguments { value: Int | String }\nattributes { result = value }",
    )
    .unwrap();
    let parsed =
        crate::config_loader::parse_directory(&path, &expression_validation_config()).unwrap();
    let child = crate::parser::parse_file(
        "arguments { value: Int | String }\nattributes { result = value }",
        &dir.path().join("child/main.crn"),
        &expression_validation_config(),
    )
    .unwrap();
    let location = child.attribute_params[0].value_location().unwrap().clone();
    let graph = deferred_types::with_session_for_test(&expression_validation_config(), |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            parsed,
            &path,
            session,
            &SchemaRegistry::new(),
        )
    })
    .unwrap();
    let types = graph.expression_report().types_at_for_test(&location);
    assert_eq!(
        types.len(),
        2,
        "one instance overwrote the other: {types:?}"
    );
    let declared = TypeExpr::Union(vec![TypeExpr::Int, TypeExpr::String]);
    assert_eq!(types, vec![declared.clone(), declared]);
}

#[test]
fn schema_bundle_replaces_findings_from_a_different_registry() {
    fn schemas(ty: crate::schema::PatternAttributeType) -> SchemaRegistry {
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            crate::schema::PatternResourceSchema::new("test.Source")
                .attribute(crate::schema::PatternAttributeSchema::optional("name", ty).build())
                .validate()
                .unwrap(),
        );
        schemas
    }
    let parsed = crate::parser::parse_and_resolve(
        "let src = mock.test.Source {}\nlet value = src.name ?? 42",
    )
    .unwrap();
    let strings = schemas(crate::schema::PatternAttributeType::string());
    let integers = schemas(crate::schema::PatternAttributeType::int());
    let first = crate::config_loader::load_buffer_configuration(
        &parsed,
        &strings,
        &expression_validation_config(),
    );
    assert_eq!(first.judged().report().findings().len(), 1);
    let second = crate::config_loader::load_buffer_configuration(
        first.parsed(),
        &integers,
        &expression_validation_config(),
    );
    assert!(
        second.judged().report().findings().is_empty(),
        "{:?}",
        second.judged().report().findings()
    );
    assert_eq!(
        second
            .judged()
            .report()
            .binding(&RelativeBindingName::new("value"))
            .unwrap()
            .static_type(),
        Ok(&TypeExpr::Int)
    );
    assert_eq!(
        first.judged().report().findings().len(),
        1,
        "the first bundle is immutable"
    );
}

#[test]
fn review2_f7_secret_coalesce_keeps_main_type_only_error() {
    let parsed =
        crate::parser::parse_and_resolve("let a = 'present'\nlet result = secret(a ?? 42)\n")
            .unwrap();
    let report = crate::parser::retained::judge_expressions(
        &parsed,
        &SchemaRegistry::new(),
        &ProviderContext::new(crate::EnvironmentSnapshot::empty()),
    );
    let error = report
        .binding(&RelativeBindingName::new("result"))
        .unwrap()
        .static_type()
        .unwrap_err();
    assert!(
        matches!(&error, crate::static_type::InferenceIssue::Failed(inference::InferenceError::IncompatibleCoalesceTypes { types }) if types.expected() == "String" && types.actual() == "Int"),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "coalesce default type `Int` does not match left type `String`"
    );
}

#[test]
fn review2_resource_type_mismatch_preserves_main_origin_text() {
    let finding = ReferenceFinding::new(
        &ReferenceFindingLocation::Resource {
            location: "mock.test.Resource.example".into(),
            attribute: "name".into(),
        },
        ReferenceFindingKind::TypeMismatch {
            types: MismatchTypeNames::for_type_exprs(&TypeExpr::String, &TypeExpr::Int),
            path: SecretDiagnosticError::Public("source.value".into()),
            origin: Some(SecretDiagnosticError::Public("from source.value".into())),
        },
    );
    assert_eq!(
        finding.to_string(),
        "mock.test.Resource.example: cannot assign Int to 'name': expected String, got Int (from source.value)"
    );
}

#[test]
fn issue_4282_deferred_builtin_results_use_existing_resolved_value_checks() {
    let identity = TypeIdentity::new(Some("mock"), ["test"], "Name");
    let schema = PatternResourceSchema::new("test.Source")
        .attribute(PatternAttributeSchema::optional("name", PatternAttributeType::string()).build())
        .attribute(
            PatternAttributeSchema::optional(
                "sink",
                PatternAttributeType::refined_string(
                    Some(identity.clone()),
                    Some(crate::schema::PatternSpec::new(
                        "^[a-z]+$",
                        crate::schema::PatternMatch::Full,
                    )),
                    Some((Some(1), Some(3))),
                    None,
                ),
            )
            .build(),
        )
        .validate()
        .unwrap();
    let scalar = TypeExpr::SchemaType {
        owned: crate::parser::ResolvedSchemaType::from_schema_declaration(
            "mock",
            schema.as_schema().attributes["sink"].attr_type(),
        )
        .unwrap(),
    };
    let config = loaded_context_for_test(ProviderContext {
        custom_types: crate::parser::ProviderCustomTypes::loaded(
            HashSet::from([crate::parser::OwnedTypeIdentity::provider(
                "mock",
                identity,
                crate::schema::RefinedScalarKind::String,
            )]),
            Box::new(|_, _| Ok(())),
        ),
        ..expression_validation_config()
    });
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema);
    for (expression, list) in [
        ("join('', [src.name, ''])", false),
        ("trim(src.name)", false),
        ("secret(join('', [src.name, '']))", false),
        ("split(',', src.name)", true),
    ] {
        let parsed = crate::parser::parse_and_resolve(&format!(
            "let src = mock.test.Source {{}}\nlet result = {expression}",
        ))
        .unwrap();
        let report = crate::parser::retained::judge_expressions(&parsed, &schemas, &config);
        let evidence = report.binding(&RelativeBindingName::new("result")).unwrap();
        let expected = if list {
            TypeExpr::List(Box::new(scalar.clone()))
        } else {
            scalar.clone()
        };
        assert_eq!(
            crate::parser::retained::admit_expression(evidence, &expected),
            Ok(crate::parser::retained::ExpressionAdmission::DeferredInterpolation),
            "{expression}",
        );
        let authored = parsed.variables["result"]
            .clone()
            .resolve_root(&RootScope::new())
            .into_expanded()
            .unwrap();
        assert!(
            matches!(
                authored
                    .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
                    .map(|proof| proof.into_classification()),
                Ok(crate::resource::knowledge::Classification::Pending(_))
            ),
            "{expression} must remain pending"
        );
        assert!(
            typecheck::check_type_expr_value(&expected, &authored, &config, evidence)
                .require_provider_schemas()
                .is_ok(),
            "{expression}"
        );
        for (input, accepted) in [("abc", true), ("abcd", false), ("123", false)] {
            let source = Resource::with_provider("mock", "test.Source", "src", None)
                .with_binding("src")
                .with_attribute("name", Value::Concrete(ConcreteValue::String(input.into())))
                .resolve_root(&RootScope::new())
                .into_expanded(&schemas)
                .unwrap();
            let bindings = crate::binding_index::ResolvedBindings::pre_apply_live(
                crate::binding_index::LivePreApplyInputs {
                    managed: &[source],
                    compositions: &[],
                    managed_current: &HashMap::new(),
                    data_source_values: &crate::data_source_value::DataSourceValues::empty(),
                    remote_bindings: &HashMap::new(),
                    wait_aliases: &[],
                },
            );
            let resolved = deferred_types::with_session_for_test(&config, |session| {
                crate::resolver::resolve_ref_value_with_config(&authored, &bindings, session)
            })
            .unwrap();
            let checked = typecheck::check_type_expr_value(&expected, &resolved, &config, evidence)
                .require_provider_schemas();
            assert_eq!(
                checked.is_ok(),
                accepted,
                "{expression}: {input}: {checked:?}"
            );
            if let Err(error) = checked {
                let message = error.to_string();
                if expression.starts_with("secret(") {
                    assert!(!message.contains(input), "{message}");
                    assert!(message.contains("secret"), "{message}");
                } else {
                    assert!(
                        message.contains(if input == "abcd" {
                            "length 4 is outside allowed range [1, 3]"
                        } else {
                            "does not match required pattern /^[a-z]+$/"
                        }),
                        "{message}"
                    );
                }
            }
        }
        assert!(!matches!(
            (authored)
                .knowledge(&crate::resource::knowledge::KnowledgeContext::validation()).map(|proof| proof.into_classification())
                .map_err(crate::resource::knowledge_serialization_error)
                .and_then(crate::resource::require_known_value)
                .map(
                    crate::resource::knowledge::Classification::<crate::resource::NoPlaceholder>::Known
                ),
            Ok(crate::resource::knowledge::Classification::Known(_))
        ));
    }
}

#[test]
fn review_r1_admission_publication_keeps_warnings_and_internal_errors() {
    for loaded in [false, true] {
        let catalog = if loaded {
            LoadedProviderContext::empty_for_test()
                .as_provider_context()
                .clone()
        } else {
            expression_validation_config()
        };
        let terminal = deferred_types::TypeCheckSession::run(&catalog, |session| {
            session.record_type(&TypeExpr::String, "admission regression");
        });
        let mut sink = Vec::new();
        publish_admission_diagnostics(terminal, &mut sink);
        assert_eq!(sink.len(), 1, "loaded={loaded}: {sink:?}");
        assert!(matches!(
            (&sink[0], loaded),
            (
                deferred_types::ProviderTypeCheckDiagnostic::Deferred(_),
                false
            ) | (
                deferred_types::ProviderTypeCheckDiagnostic::UnsettledLoadedCheck(_),
                true
            )
        ));
    }
}

#[test]
fn review_r3_interpolation_reports_every_computed_access_error() {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Source")
            .attribute(
                PatternAttributeSchema::optional("first", PatternAttributeType::string()).build(),
            )
            .attribute(
                PatternAttributeSchema::optional("second", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        PatternResourceSchema::new("test.Sink")
            .attribute(
                PatternAttributeSchema::optional("value", PatternAttributeType::string()).build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse("let src = mock.test.Source {}\nlet sink = mock.test.Sink { value = \"${[src.first][0][0]}-${[src.second][0][0]}\" }\n", &expression_validation_config()).unwrap();
    let error = validate_resource_ref_types_for_test(&parsed, &schemas).unwrap_err();
    assert_eq!(error.lines().count(), 2, "{error}");
}
