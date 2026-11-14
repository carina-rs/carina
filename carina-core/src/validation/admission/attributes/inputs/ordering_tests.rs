use crate as carina_core;
use carina_core::binding_index::BindingIndex;
use carina_core::config_loader::{load_buffer_configuration, parse_directory};
use carina_core::parser::{AnalysisTree, ParsedFile, ProviderContext};
use carina_core::paths::ResolvedPath;
use carina_core::resource::knowledge::KnowledgeContext;
use carina_core::resource::{ConcreteValue, Value};
use carina_core::schema::{
    AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, TypeError,
};
use carina_core::validation::admission::attributes::ResourceAttributeInputs;
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
fn classifications_keep_authored_attribute_order() {
    let parsed = directory("unknown_attributes");
    let schemas = schemas();
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let loaded = load_buffer_configuration(&parsed, &schemas, &config);
    let tree = AnalysisTree::try_from_file(loaded.judged().file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let inputs = ResourceAttributeInputs::for_resource(resource, loaded.judged().report()).unwrap();
    let classified = inputs.classify(&KnowledgeContext::validation(), |_, error| {
        panic!("{error}")
    });
    assert_eq!(
        classified.keys().map(String::as_str).collect::<Vec<_>>(),
        std::iter::once("name").chain(UNKNOWN).collect::<Vec<_>>(),
    );
}

#[test]
fn admission_uses_owning_resource_order_even_for_reversed_classifications() {
    let parsed = directory("unknown_attributes");
    let schemas = schemas();
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let loaded = load_buffer_configuration(&parsed, &schemas, &config);
    let judged = loaded.judged();
    let tree = AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let inputs = ResourceAttributeInputs::for_resource(resource, judged.report()).unwrap();
    let classified = inputs.classify(&KnowledgeContext::validation(), |_, error| {
        panic!("{error}")
    });
    let mut entries = classified.into_iter().collect::<Vec<_>>();
    entries.reverse();
    let classified = entries.into_iter().collect();
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &config);
    with_session_for_test(&config, |session| {
        let report = super::super::typed_resource_attribute_admissions::<
            crate::validation::admission::kind::ResourceAttribute,
            _,
            _,
            _,
        >(
            resource,
            judged.file(),
            resource.schema(&schemas).unwrap(),
            &classified,
            resource.quoted_string_attrs(),
            &inputs,
            &bindings,
            session,
            &schemas,
            &std::cell::RefCell::new(Default::default()),
        );
        assert_eq!(
            report
                .errors()
                .map(|error| match error {
                    TypeError::UnknownAttribute { name, .. } => name.as_str(),
                    error => panic!("unexpected finding: {error}"),
                })
                .collect::<Vec<_>>(),
            UNKNOWN
        );
    });
}

#[test]
fn resolved_and_reported_inputs_order_aliases_and_replacements_by_the_owner() {
    let parsed = custom_directory(
        "mock.test.resource { name = 'ordered' cat = ['first'] zulu_knob = 'z' bat = ['second'] }",
    );
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        ResourceSchema::new("test.resource")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .attribute(
                AttributeSchema::optional(
                    "first_group",
                    AttributeType::list(AttributeType::string()),
                )
                .with_block_name("cat")
                .build(),
            )
            .attribute(
                AttributeSchema::optional(
                    "second_group",
                    AttributeType::list(AttributeType::string()),
                )
                .with_block_name("bat")
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
    let aliases = resource
        .schema(&schemas)
        .unwrap()
        .schema()
        .as_schema()
        .block_name_map();
    let mut values: indexmap::IndexMap<_, _> = resource
        .attributes()
        .iter()
        .rev()
        .map(|(name, value)| {
            (
                aliases.get(name).unwrap_or(name).clone(),
                if name == "cat" {
                    Value::Concrete(ConcreteValue::List(vec![Value::Concrete(
                        ConcreteValue::String("replacement".into()),
                    )]))
                } else {
                    value.clone()
                },
            )
        })
        .collect();
    values.insert(
        "report_only".to_owned(),
        Value::Concrete(ConcreteValue::String("extra".into())),
    );
    let resolved =
        ResourceAttributeInputs::from_resolved(resource, &values, judged.report()).unwrap();
    let reported = ResourceAttributeInputs::from_report(resource, &values, judged.report());
    assert!(resolved.authored_evidence().is_ok());
    assert!(reported.authored_evidence().is_err());
    for inputs in [&resolved, &reported] {
        assert_eq!(
            inputs.values().map(|(name, _)| name).collect::<Vec<_>>(),
            [
                "name",
                "first_group",
                "zulu_knob",
                "second_group",
                "report_only"
            ]
        );
        for (name, value) in &values {
            assert_eq!(inputs.value(name), Some(value));
        }
    }
    assert_ne!(
        resolved.value("first_group"),
        resource.authored_slot("cat").unwrap().projection()
    );
}

#[test]
fn materialized_subsets_keep_owner_order_and_unchanged_projections() {
    let parsed = directory("unknown_attributes");
    let schemas = schemas();
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let loaded = load_buffer_configuration(&parsed, &schemas, &config);
    let tree = AnalysisTree::try_from_file(loaded.judged().file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let mut inputs =
        ResourceAttributeInputs::for_resource(resource, loaded.judged().report()).unwrap();
    let original = inputs.fork();
    let values = ["delta_knob", "quartz_knob", "zulu_knob"]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                Value::Concrete(ConcreteValue::String(format!("replacement-{name}").into())),
            )
        })
        .collect();
    let (updated, classified) =
        inputs.materialize_and_classify(&values, &KnowledgeContext::validation(), |_, error| {
            panic!("{error}")
        });
    assert_eq!(
        classified.keys().map(String::as_str).collect::<Vec<_>>(),
        ["zulu_knob", "quartz_knob", "delta_knob"]
    );
    assert_eq!(
        updated.values().map(|(name, _)| name).collect::<Vec<_>>(),
        std::iter::once("name").chain(UNKNOWN).collect::<Vec<_>>()
    );
    for (name, value) in &values {
        assert_eq!(updated.value(name), Some(value));
    }
    assert!(std::ptr::eq(
        updated.value("alpha_knob").unwrap(),
        original.value("alpha_knob").unwrap()
    ));

    let additions = ["extra_z", "extra_a"]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                Value::Concrete(ConcreteValue::String(name.into())),
            )
        })
        .collect();
    let (updated, classified) =
        inputs.materialize_and_classify(&additions, &KnowledgeContext::validation(), |_, error| {
            panic!("{error}")
        });
    assert_eq!(
        classified.keys().map(String::as_str).collect::<Vec<_>>(),
        ["extra_z", "extra_a"]
    );
    assert_eq!(
        updated.values().map(|(name, _)| name).collect::<Vec<_>>(),
        std::iter::once("name")
            .chain(UNKNOWN)
            .chain(["extra_z", "extra_a"])
            .collect::<Vec<_>>()
    );
}

#[test]
fn projected_attributes_take_resource_order_without_authored_authority() {
    let mut parsed = custom_directory(
        "mock.test.resource { name_prefix = 'prefix-' zulu_knob = 'z' alpha_knob = 'a' }",
    );
    let schemas = schemas();
    carina_core::identifier::resolve_attr_prefixes(&mut parsed.resources, &schemas).unwrap();
    let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let loaded = load_buffer_configuration(&parsed, &schemas, &config);
    let tree = AnalysisTree::try_from_file(loaded.judged().file()).unwrap();
    let resource = tree.resources().next().unwrap();
    assert!(resource.authored_slot("name").is_err());
    let values = resource
        .attributes()
        .iter()
        .rev()
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let inputs =
        ResourceAttributeInputs::from_resolved(resource, &values, loaded.judged().report())
            .unwrap();
    assert_eq!(
        inputs
            .values()
            .map(|(name, _)| name.to_owned())
            .collect::<Vec<_>>(),
        resource.attributes().keys().cloned().collect::<Vec<_>>()
    );
    assert_eq!(inputs.value("name"), resource.attributes().get("name"));
}
