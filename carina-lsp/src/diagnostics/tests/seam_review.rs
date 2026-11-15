use super::*;

fn projection_selection_case(ambiguous: bool) {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let parsed = carina_core::parser::parse_file(
        "mock.Test { input = 'folded' }",
        Path::new("test.crn"),
        &catalog,
    )
    .unwrap();
    let configuration =
        carina_core::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &catalog);
    let owners = configuration.analysis_tree().unwrap();
    let owner = owners.resources().next().unwrap();
    let mut source = if ambiguous {
        parsed.clone()
    } else {
        carina_core::parser::parse_file(
            "mock.Test { input = 'foreign' }",
            Path::new("other.crn"),
            &catalog,
        )
        .unwrap()
    };
    if ambiguous {
        let duplicate = source.resources[0].clone();
        source.resources.push(duplicate); // allow: direct — fixture test inspection
    }
    let source = carina_core::parser::AnalysisTree::try_from_file(&source).unwrap();
    let authored = carina_core::parser::AnalysisTree::try_from_file(&parsed).unwrap();
    let result = select_resource_projection(
        owner,
        Some(&source),
        if ambiguous { &authored } else { &source },
    )
    .0;
    if ambiguous {
        assert!(
            matches!(result, ResourceProjection::Ambiguous),
            "ambiguity must not choose the first or use a different view"
        );
    } else {
        assert!(
            matches!(result, ResourceProjection::NoMatch),
            "a missing per-file owner must not become folded merged values"
        );
    }
}

#[test]
fn semantic_projection_miss_never_uses_merged_values() {
    projection_selection_case(false);
}

#[test]
fn semantic_projection_ambiguity_never_uses_first_source() {
    projection_selection_case(true);
}

#[test]
fn semantic_projection_missing_owner_publishes_evidence_failure() {
    projection_failure_is_visible(false);
}

#[test]
fn semantic_projection_ambiguous_owner_publishes_evidence_failure() {
    projection_failure_is_visible(true);
}

fn projection_failure_is_visible(ambiguous: bool) {
    let doc = create_document("mock.Test { input = 'authored' }");
    let public = doc.for_diagnostics().unwrap();
    let tree = carina_core::parser::AnalysisTree::try_from_file(doc.parsed().unwrap()).unwrap();
    let owner = tree.resources().next().unwrap();
    let projection = if ambiguous {
        ResourceProjection::<RelativeBindingName>::Ambiguous
    } else {
        ResourceProjection::NoMatch
    };
    let mut diagnostics = Vec::new();
    assert!(
        projection_or_diagnostic(
            &public,
            owner.id(),
            owner.type_location(),
            projection,
            &mut diagnostics,
        )
        .is_none()
    );
    assert_eq!(
        diagnostics.len(),
        1,
        "missing correspondence must fail closed"
    );
    assert_eq!(
        diagnostics[0].message,
        carina_core::parser::retained::MissingEvidence::for_resource(owner.id()).to_string()
    );
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert_eq!(
        diagnostics[0].range,
        Range::new(Position::new(0, 0), Position::new(0, 9))
    );
}

#[test]
fn semantic_projection_analysis_failure_keeps_main_silence() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let authored = carina_core::parser::parse_file(
        "mock.Test { input = 'authored' }",
        Path::new("test.crn"),
        &catalog,
    )
    .unwrap();
    let primary = carina_core::parser::parse_file(
        "mock.Test { input = 'folded' }",
        Path::new("test.crn"),
        &catalog,
    )
    .unwrap();
    let configuration =
        carina_core::test_utils::load_canonical_buffer_configuration(&primary, &schemas, &catalog);
    let owners = configuration.analysis_tree().unwrap();
    let owner = owners.resources().next().unwrap();
    let mut invalid = authored.clone();
    invalid.resources[0].declaration = Default::default();
    // The optional helper takes no document or diagnostic sink, so failure
    // cannot publish the extra invariant diagnostic seen in the red test.
    let semantic = semantic_tree_for_values(&invalid);
    assert!(semantic.is_none());
    let authored = carina_core::parser::AnalysisTree::try_from_file(&authored).unwrap();
    match select_resource_projection(owner, semantic.as_ref(), &authored).0 {
        ResourceProjection::Matched(projection) => assert_eq!(
            projection.values()["input"],
            carina_core::resource::Value::Concrete(carina_core::resource::ConcreteValue::String(
                "authored".into()
            ))
        ),
        ResourceProjection::NoMatch | ResourceProjection::Ambiguous => {
            panic!("main uses the valid authored view when the per-file semantic view fails")
        }
    }
}

#[test]
fn second_file_resource_function_call_keeps_main_guidance() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "test",
        ResourceSchema::new("Widget")
            .attribute(AttributeSchema::optional("value", AttributeType::bool()).build())
            .validate()
            .unwrap(),
    );
    let engine = custom_engine(schemas);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("a.crn"),
        "provider test {}\nlet input = 'BAD'\nlet first = test.Widget { value = true }\n",
    )
    .unwrap();
    let source = "let second = test.Widget { value = lower(input) }\n";
    let path = directory.path().join("z.crn");
    std::fs::write(&path, source).unwrap();
    let doc = create_document_at(source, path.clone());
    let diagnostics = engine.analyze(&doc, Some(&path));
    let guidance = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.message.contains("Use true or false"))
        .collect::<Vec<_>>();
    assert_eq!(guidance.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        guidance[0].message,
        "Type mismatch: expected Bool, got string 'bad'. Use true or false."
    );
    assert_eq!(
        guidance[0].range,
        Range::new(Position::new(0, 27), Position::new(0, 32))
    );
}

#[test]
fn review_seam_loop_lookup_preserves_every_instance() {
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let parsed = carina_core::parser::parse_file(
        "for item in ['first', 'second'] { mock.Test { input = item } }",
        Path::new("main.crn"),
        &catalog,
    )
    .unwrap();
    let schemas = SchemaRegistry::new();
    let configuration =
        carina_core::test_utils::load_canonical_buffer_configuration(&parsed, &schemas, &catalog);
    let tree = configuration.parsed().analysis_tree().unwrap();
    let resources: HashMap<_, _> = tree
        .resources()
        .filter_map(|resource| resource.authored_instance_key().map(|key| (key, resource)))
        .collect();
    assert_eq!(
        resources.len(),
        2,
        "a declaration has two distinct cursor roots"
    );
    for resource in tree.project_resources() {
        assert!(
            resources
                .values()
                .any(|candidate| candidate.id() == resource.id())
        );
    }
}

#[test]
fn review_seam_invalid_judged_tree_becomes_a_diagnostic() {
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut parsed = carina_core::parser::parse_file(
        "mock.Test { input = 'x' }",
        Path::new("main.crn"),
        &catalog,
    )
    .unwrap();
    parsed.resources[0].declaration = Default::default();
    let doc = create_document("mock.Test { input = 'x' }");
    let doc = doc.for_diagnostics().expect("public fixture");
    let mut diagnostics = Vec::new();
    assert!(analysis_tree(&doc, &parsed, &mut diagnostics).is_none());
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].severity, Some(DiagnosticSeverity::ERROR));
    assert!(
        diagnostics[0]
            .message
            .contains("missing declaration provenance")
    );
}

#[test]
fn review_seam_expanded_admission_retains_instantiated_judgements() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("child")).unwrap();
    std::fs::write(directory.path().join("main.crn"),
        "let child = use { source = './child' }\nlet first = child { input = 'one' }\nlet second = child { input = 'two' }\n").unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        "arguments { input: String }\nmock.Test { input = input }\n",
    )
    .unwrap();
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let path = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
    let root = carina_core::config_loader::parse_directory(&path, &catalog).unwrap();
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("input", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    carina_core::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let graph = carina_core::module_resolver::resolve_modules_with_providerless_config(
            root.admit(&schemas, session),
            &path,
        )
        .unwrap();
        let configuration = carina_core::test_utils::load_canonical_buffer_configuration(
            &graph, &schemas, &catalog,
        );
        let judged = configuration.judged();
        let instances = &graph;
        let tree = judged.analysis_tree().unwrap();
        let mut inputs = ExpandedDiagnosticValidationInputs {
            configuration: judged,
            resources: tree.resources().collect(),
            binding_index: judged.parsed().binding_index(&catalog),
        };
        assert_eq!(instances.resources.len(), 2);
        for resource in &instances.resources {
            let findings = inputs
                .resource_admissions(&resource.id, session)
                .unwrap()
                .unwrap();
            assert_eq!(
                findings.errors().count(),
                0,
                "{}: {findings:?}",
                resource.id
            );
        }
    });
}
