use super::*;

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
        carina_core::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let tree =
        carina_core::parser::AnalysisTree::try_from_file(configuration.judged().file()).unwrap();
    let resources = authored_validation_resources(&tree);
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
            root, &path, session, &schemas,
        )
        .unwrap();
        let configuration =
            carina_core::config_loader::load_buffer_configuration(&graph, &schemas, &catalog);
        let judged = configuration.judged();
        let instances = judged.file().clone();
        let tree = carina_core::parser::AnalysisTree::try_from_file(&instances).unwrap();
        let inputs = ExpandedDiagnosticValidationInputs {
            configuration: judged,
            resources: tree
                .resources()
                .map(|resource| (resource.id(), resource))
                .collect(),
            coverage: Default::default(),
            binding_index: carina_core::binding_index::BindingIndex::from_parsed(
                judged.file(),
                &schemas,
                &catalog,
            ),
        };
        assert_eq!(instances.resources.len(), 2);
        for resource in &instances.resources {
            let findings = inputs.resource_admissions(&resource.id, session).unwrap();
            assert_eq!(
                findings.errors().count(),
                0,
                "{}: {findings:?}",
                resource.id
            );
        }
    });
}
