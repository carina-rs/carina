use super::*;
use std::cell::Cell;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fault {
    None,
    NoTree,
    NoKeys,
}
thread_local! { static FAULT: Cell<Fault> = const { Cell::new(Fault::None) }; }
pub(crate) fn fault() -> Fault {
    FAULT.get()
}
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        FAULT.set(Fault::None);
    }
}

fn check(name: &str, fault: Fault) {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::optional("input", AttributeType::int()).build())
            .validate()
            .unwrap(),
    );
    let engine = custom_engine(schemas);
    let doc = create_document(if name == "no_keys_valid" {
        "provider mock {}\nmock.Test { input = 1 }\n"
    } else {
        "provider mock {}\nmock.Test { input = 'bad' }\n"
    });
    FAULT.set(fault);
    let _reset = Reset;
    let actual = serde_json::to_value(engine.analyze(&doc, None)).unwrap();
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("seam_round2_main.json")).unwrap();
    assert_eq!(actual, expected[name], "{name}");
}

#[test]
fn round2_lsp_missing_authored_tree_keeps_main_schema_diagnostics() {
    check("no_tree", Fault::NoTree);
}
#[test]
fn round2_lsp_missing_instance_key_keeps_main_schema_diagnostics() {
    check("no_keys", Fault::NoKeys);
}

#[test]
fn round2_lsp_missing_expanded_report_selects_main_fallback() {
    use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema};
    let mut actual = Vec::<Option<Vec<String>>>::new();
    for (argument_type, expression, target) in [
        (
            "map(map(Int))",
            "input['nested']['count']",
            AttributeType::int(),
        ),
        ("Int", "input ?? 1", AttributeType::int()),
        ("Int", "input + 1", AttributeType::int()),
        ("String", "\"${input}\"", AttributeType::string()),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(directory.path().join("main.crn"), format!("arguments {{ input: {argument_type} }}\nprovider mock {{}}\nlet child = use {{ source = './child' }}\nlet first = child {{ input = input }}\n")).unwrap();
        std::fs::write(
            directory.path().join("child/main.crn"),
            format!(
                "arguments {{ input: {argument_type} }}\nmock.Test {{ input = {expression} }}\n"
            ),
        )
        .unwrap();
        let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let mut schemas = SchemaRegistry::new();
        schemas.insert(
            "mock",
            ResourceSchema::new("Test")
                .attribute(AttributeSchema::optional("input", target).build())
                .validate()
                .unwrap(),
        );
        let path = carina_core::paths::ResolvedPath::new(directory.path()).unwrap();
        let parsed = carina_core::config_loader::parse_directory(&path, &config).unwrap();
        carina_core::validation::deferred_types::with_session_for_test(&config, |session| {
            let graph = carina_core::module_resolver::resolve_modules_with_providerless_config(
                parsed, &path, session, &schemas,
            )
            .unwrap();
            let configuration =
                carina_core::config_loader::load_buffer_configuration(&graph, &schemas, &config);
            let mut instances = configuration.judged().file().clone();
            for resource in &mut instances.resources {
                // allow: direct - emulate an expanded instance absent from the original report
                resource.id = carina_core::resource::ResourceId::with_provider_identity(
                    "mock",
                    "Test",
                    "unreported",
                    None,
                );
            }
            let tree = carina_core::parser::AnalysisTree::try_from_file(&instances).unwrap();
            let mut inputs = ExpandedDiagnosticValidationInputs {
                configuration: configuration.judged(),
                resources: tree
                    .resources()
                    .map(|resource| (resource.id(), resource))
                    .collect(),
                coverage: Default::default(), // SEAM_ONLY_COVERAGE
                binding_index: carina_core::binding_index::BindingIndex::from_parsed(
                    configuration.judged().file(),
                    &schemas,
                    &config,
                ),
            };
            let resource = tree.resources().next().unwrap();
            let result = inputs.resource_admissions(resource.id(), session);
            actual.push(result.map(|report| {
                report
                    .errors()
                    .chain(report.symbolic_errors().map(|(_, _, error)| error))
                    .map(ToString::to_string)
                    .collect()
            }));
        });
    }
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("seam_round2_main.json")).unwrap();
    let actual = serde_json::json!(actual);
    assert_eq!(actual, expected["no_expanded_report"]);
}

#[test]
fn round2_lsp_missing_key_does_not_reject_main_accepted_input() {
    check("no_keys_valid", Fault::NoKeys);
}
