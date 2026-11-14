use carina_core::binding_index::BindingIndex;
use carina_core::config_loader::{load_buffer_configuration, parse_directory};
use carina_core::parser::{AnalysisTree, ParsedFile, ProviderContext};
use carina_core::paths::ResolvedPath;
use carina_core::resource::knowledge::KnowledgeContext;
use carina_core::resource::{ConcreteValue, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeError, suggest_similar_name,
};
use carina_core::validation::admission::attributes::{
    ResourceAttributeInputs, resource_attribute_admissions,
};
use carina_core::validation::deferred_types::with_session_for_test;

const UNKNOWN: [&str; 6] = [
    "zulu_knob",
    "alpha_knob",
    "quartz_knob",
    "bravo_knob",
    "mango_knob",
    "delta_knob",
];

fn directory(name: &str) -> ParsedFile {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/diagnostic_order")
        .join(name);
    parse_directory(
        &ResolvedPath::new(root).unwrap(),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .into_parsed()
}

fn custom_directory(source: &str) -> ParsedFile {
    let root = tempfile::tempdir().unwrap();
    for (name, source) in [
        ("main.crn", source),
        ("providers.crn", "provider mock {}"),
        (
            "backend.crn",
            "backend local { path = 'carina.state.json' }",
        ),
        ("exports.crn", "exports {}"),
    ] {
        std::fs::write(root.path().join(name), source).unwrap();
    }
    parse_directory(
        &ResolvedPath::new(root.path()).unwrap(),
        &ProviderContext::new(carina_core::EnvironmentSnapshot::empty()),
    )
    .unwrap()
    .into_parsed()
}

fn schemas() -> SchemaRegistry {
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    schemas
}

#[test]
fn functions_keep_directory_declaration_order_after_forward_resolution() {
    let parsed = directory("functions");
    assert_eq!(
        parsed
            .user_functions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["zebra", "mango", "alpha", "bravo"],
    );
    assert_eq!(
        parsed.variables["result"],
        Value::Concrete(ConcreteValue::String("works".into()))
    );
}

#[test]
fn function_annotation_findings_keep_sibling_file_order() {
    let mut parsed = directory("invalid_annotations");
    let catalog = carina_core::parser::LoadedProviderContext::empty_for_test();
    with_session_for_test(catalog.as_provider_context(), |session| {
        let findings =
            carina_core::validation::resolve_file_type_exprs(&mut parsed, session).unwrap();
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.detail().to_string())
                .collect::<Vec<_>>(),
            [
                "function parameter 'zebra' parameter 'value': unknown custom type 'mock.test.sdk_validator.Guarded'",
                "function parameter 'alpha' parameter 'value': unknown custom type 'mock.test.sdk_validator.Guarded'",
            ],
        );
        assert_eq!(
            findings
                .iter()
                .map(|finding| (
                    finding
                        .location()
                        .file()
                        .path()
                        .file_name()
                        .unwrap()
                        .to_str()
                        .unwrap(),
                    finding.location().span().start_line(),
                ))
                .collect::<Vec<_>>(),
            [("a_functions.crn", 1), ("z_functions.crn", 1)],
        );
    });
}

const FUNCTION_LOCATIONS: [(&str, usize); 4] = [
    ("a_functions.crn", 1),
    ("a_functions.crn", 2),
    ("z_functions.crn", 1),
    ("z_functions.crn", 2),
];

fn finding_location(location: &carina_core::parser::DeclarationLocation) -> (String, usize) {
    (
        location
            .file()
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned(),
        location.span().start_line(),
    )
}

#[test]
fn function_arithmetic_findings_keep_sibling_declaration_order() {
    let parsed = directory("functions/arithmetic");
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schemas = schemas();
    let loaded = load_buffer_configuration(&parsed, &schemas, &config);
    let bindings = BindingIndex::from_parsed(loaded.judged().file(), &schemas, &config);
    let findings =
        carina_core::validation::check_file_integer_arithmetic_values(loaded.judged(), &bindings);
    assert_eq!(findings.len(), 4, "{findings:?}");
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding_location(finding.location()))
            .collect::<Vec<_>>(),
        FUNCTION_LOCATIONS.map(|(file, line)| (file.to_owned(), line))
    );
    for finding in findings {
        assert!(
            finding
                .detail()
                .to_string()
                .contains("left operand must be Int, got Float"),
            "{finding}"
        );
    }
}

#[test]
fn function_reference_and_call_findings_keep_sibling_declaration_order() {
    let parsed = directory("functions/references");
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let schemas = schemas();
    let loaded = load_buffer_configuration(&parsed, &schemas, &config);
    let bindings = BindingIndex::from_parsed(loaded.judged().file(), &schemas, &config);
    let errors =
        carina_core::validation::validate_user_function_ref_types_with_bindings(&parsed, &bindings)
            .unwrap_err();
    let errors = errors.lines().collect::<Vec<_>>();
    assert_eq!(errors.len(), 4, "{errors:#?}");
    for (error, function) in errors.iter().zip(["zebra", "sink", "alpha", "sink"]) {
        assert!(error.contains(&format!("function '{function}'")), "{error}");
        assert!(
            error.contains("expected Int") && error.contains("got String"),
            "{error}"
        );
    }
    let tree = AnalysisTree::try_from_file(loaded.judged().file()).unwrap();
    let findings = carina_core::validation::user_function_reference_diagnostics(&tree, &bindings);
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding_location(finding.location()))
            .collect::<Vec<_>>(),
        FUNCTION_LOCATIONS.map(|(file, line)| (file.to_owned(), line))
    );
    assert_eq!(
        findings
            .iter()
            .map(|finding| finding.detail().as_str())
            .collect::<Vec<_>>(),
        errors
    );
}

#[test]
fn retained_function_references_keep_sibling_declaration_order() {
    let parsed = directory("functions/references");
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    assert_eq!(
        tree.value_references()
            .iter()
            .map(|reference| reference.context())
            .collect::<Vec<_>>(),
        [
            "function zebra",
            "function mango",
            "function alpha",
            "function bravo"
        ]
    );
    assert_eq!(
        tree.value_references()
            .iter()
            .map(|reference| finding_location(reference.reference().location()))
            .collect::<Vec<_>>(),
        FUNCTION_LOCATIONS.map(|(file, line)| (file.to_owned(), line))
    );
}

#[test]
fn retained_function_annotations_keep_sibling_declaration_order() {
    let parsed = directory("functions/references");
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    assert_eq!(
        tree.type_annotations()
            .iter()
            .map(|annotation| annotation.detail().to_string())
            .collect::<Vec<_>>(),
        [
            "function return type 'zebra'",
            "function return type 'mango'",
            "function return type 'alpha'",
            "function return type 'bravo'",
            "function parameter 'sink' parameter 'value'",
            "function return type 'sink'",
        ]
    );
}

#[test]
fn duplicate_sibling_functions_keep_the_existing_last_definition_and_rejection() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/diagnostic_order/functions/duplicates");
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let parsed = parse_directory(&ResolvedPath::new(&root).unwrap(), &config).unwrap();
    assert_eq!(
        parsed
            .parsed()
            .user_functions
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["zebra", "mango", "alpha"]
    );
    assert_eq!(
        parsed.parsed().variables["result"],
        Value::Concrete(ConcreteValue::String("last".into()))
    );
    let loaded = carina_core::config_loader::load_configuration_for_bootstrap(
        &ResolvedPath::new(&root).unwrap(),
        &config,
        &schemas(),
    )
    .unwrap();
    let duplicates = loaded.duplicate_declarations;
    assert_eq!(
        duplicates
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["duplicate function 'zebra': declared in a_functions.crn and z_functions.crn"]
    );
}

#[test]
fn block_alias_suggestion_ties_choose_lexical_minimum() {
    let schema = ResourceSchema::new("Aliases")
        .attribute(
            AttributeSchema::optional("first_group", AttributeType::list(AttributeType::string()))
                .with_block_name("cat")
                .build(),
        )
        .attribute(
            AttributeSchema::optional("second_group", AttributeType::list(AttributeType::string()))
                .with_block_name("bat")
                .build(),
        )
        .validate()
        .unwrap();
    for _ in 0..64 {
        let aliases = schema.as_schema().block_name_map();
        assert_eq!(
            aliases.keys().map(String::as_str).collect::<Vec<_>>(),
            ["cat", "bat"]
        );
        let primary = schema
            .as_schema()
            .attributes()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        let candidates = aliases.keys().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            suggest_similar_name("hat", &primary, &candidates).as_deref(),
            Some("bat")
        );
    }
    assert_eq!(
        suggest_similar_name("hat", &["bat", "cat"], &[]).as_deref(),
        Some("bat")
    );
}

#[test]
fn suggestion_ties_choose_lexical_minimum_for_every_candidate_order() {
    for candidates in [["cat", "bat"], ["bat", "cat"]] {
        assert_eq!(
            suggest_similar_name("hat", &candidates, &[]).as_deref(),
            Some("bat")
        );
    }
}

#[test]
fn canonical_attribute_wins_an_equal_distance_block_alias() {
    let parsed = custom_directory("mock.test.resource { name = 'ordered' rulez = [] }");
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional("rules", AttributeType::list(AttributeType::string()))
                    .with_block_name("rule")
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let loaded = load_buffer_configuration(&parsed, &schemas, &config);
    let judged = loaded.judged();
    let tree = AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let inputs = ResourceAttributeInputs::for_resource(resource, judged.report()).unwrap();
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &config);
    with_session_for_test(&config, |session| {
        let report = {
            let outcome = resource_attribute_admissions(
                resource,
                judged.file(),
                resource.schema(&schemas).unwrap(),
                resource.quoted_string_attrs(),
                inputs,
                carina_core::validation::admission::attributes::ValidationEvaluation::None,
                &bindings,
                session,
                &schemas,
                &mut Default::default(),
            );
            assert!(outcome.classification_diagnostics.0.is_empty());
            outcome.values
        };
        let errors = report.errors().collect::<Vec<_>>();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            matches!(errors[0], TypeError::UnknownAttribute { name, suggestion }
            if name == "rulez" && suggestion.as_deref() == Some("rules")),
            "{errors:?}"
        );
    });
}

#[test]
fn module_arguments_and_their_source_spans_keep_authored_order() {
    for _ in 0..8 {
        let parsed = directory("module_arguments/unknown");
        let call = &parsed.module_calls[0];
        let expected = [
            "flag",
            "zulu_bad",
            "alpha_bad",
            "quartz_bad",
            "bravo_bad",
            "mango_bad",
            "delta_bad",
        ];
        assert_eq!(
            call.arguments().map(|(name, _)| name).collect::<Vec<_>>(),
            expected
        );
        assert_eq!(
            call.arguments_with_source_spans()
                .map(|argument| (argument.name(), argument.key_span().start_line()))
                .collect::<Vec<_>>(),
            expected
                .into_iter()
                .enumerate()
                .map(|(index, name)| (name, index + 2))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn retained_module_argument_references_keep_authored_order() {
    let parsed = directory("module_arguments/references");
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    assert_eq!(
        tree.value_references()
            .iter()
            .map(|value| value
                .reference()
                .render(|path| path.binding().to_string())
                .to_string())
            .collect::<Vec<_>>(),
        [
            "missing_zulu",
            "missing_alpha",
            "missing_quartz",
            "missing_bravo",
            "missing_mango",
            "missing_delta"
        ]
    );
}

#[test]
fn plan_admission_validates_supplied_proofs_absent_from_the_resource() {
    use carina_core::resource::knowledge::PlanResolved;
    use carina_core::validation::admission::{attributes::admit_plan_attributes, kind};
    let resource: carina_core::resource::Resource =
        carina_core::resource::Resource::with_provider("mock", "Sink", "sink", None);
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Sink")
            .attribute(AttributeSchema::optional("extra", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let value = Value::Concrete(ConcreteValue::Int(1));
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let context = KnowledgeContext::plan(&bindings);
    let proofs = [(
        "extra".to_owned(),
        PlanResolved::try_from(value.knowledge(&context).unwrap()).unwrap(),
    )]
    .into_iter()
    .collect();
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    with_session_for_test(&config, |session| {
        let report = admit_plan_attributes::<kind::ResourceAttribute>(
            schemas.get_for(&resource).unwrap(),
            &resource.id,
            &proofs,
            &Default::default(),
            session,
        );
        assert_eq!(
            report
                .diagnostic_errors()
                .map(|(_, path, _)| path.unwrap().0)
                .collect::<Vec<_>>(),
            ["extra"]
        );
    });
}

#[test]
fn plan_admission_keeps_supplied_proof_order() {
    use carina_core::resource::knowledge::PlanResolved;
    use carina_core::validation::admission::{attributes::admit_plan_attributes, kind};
    let mut resource: carina_core::resource::Resource =
        carina_core::resource::Resource::with_provider("mock", "Sink", "sink", None);
    let mut schema = ResourceSchema::new("Sink");
    for name in UNKNOWN {
        resource = resource.with_attribute(name, Value::Concrete(ConcreteValue::Int(1)));
        schema = schema.attribute(AttributeSchema::optional(name, AttributeType::string()).build());
    }
    let mut schemas = SchemaRegistry::new();
    schemas.insert("mock", schema.validate().unwrap());
    let bindings = carina_core::binding_index::ResolvedBindings::default();
    let context = KnowledgeContext::plan(&bindings);
    let proofs = resource
        .attributes
        .iter()
        .rev()
        .map(|(name, value)| {
            (
                name.clone(),
                PlanResolved::try_from(value.knowledge(&context).unwrap()).unwrap(),
            )
        })
        .collect();
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    with_session_for_test(&config, |session| {
        let report = admit_plan_attributes::<kind::ResourceAttribute>(
            schemas.get_for(&resource).unwrap(),
            &resource.id,
            &proofs,
            &Default::default(),
            session,
        );
        assert_eq!(
            report
                .diagnostic_errors()
                .map(|(error, path, _)| path.unwrap_or_else(|| panic!("missing path: {error:?}")).0)
                .collect::<Vec<_>>(),
            UNKNOWN.into_iter().rev().collect::<Vec<_>>()
        );
    });
}

#[test]
fn unknown_module_argument_findings_keep_authored_order() {
    let parsed = directory("module_arguments/unknown");
    let tree = AnalysisTree::try_from_file(&parsed).unwrap();
    let base = ResolvedPath::new(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/diagnostic_order/module_arguments/unknown"),
    )
    .unwrap();
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    with_session_for_test(&context, |session| {
        let findings = carina_core::validation::check_module_calls_from_analysis(
            &tree,
            &base,
            session,
            &SchemaRegistry::new(),
            &Default::default(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.message().to_string())
                .collect::<Vec<_>>(),
            ["zulu", "alpha", "quartz", "bravo", "mango", "delta"]
                .map(|name| format!("Unknown argument '{name}_bad' for module 'choice'"))
        );
        assert_eq!(
            findings
                .iter()
                .map(|finding| finding.anchor().span.start_line())
                .collect::<Vec<_>>(),
            [3, 4, 5, 6, 7, 8]
        );
    });
}
