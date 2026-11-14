use carina_core::binding_index::BindingIndex;
use carina_core::config_loader::load_buffer_configuration;
use carina_core::parser::{AnalysisTree, ProviderContext, parse_file_and_resolve};
use carina_core::resource::{ConcreteValue, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use carina_core::validation::admission::attributes::{
    ResourceAttributeInputs, resource_attribute_admissions,
};
use carina_core::validation::deferred_types::with_session_for_test;

fn rejects_invalid_product_value(projected: bool) {
    let catalog = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let mut parsed = parse_file_and_resolve(
        if projected {
            "mock.Test { name_prefix = 'valid-' }"
        } else {
            "mock.Test { name = 'valid' }"
        },
        std::path::Path::new("admission.crn"),
        &catalog,
    )
    .unwrap();
    if projected {
        carina_core::identifier::resolve_attr_prefixes(&mut parsed.resources, &schemas).unwrap();
    }
    let configuration = load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let mut values = resource.attributes().into_owned();
    values.insert("name".into(), Value::Concrete(ConcreteValue::Int(7)));
    let inputs = if projected {
        assert!(resource.authored_slot("name").is_err());
        ResourceAttributeInputs::from_resolved(resource, &values, judged.report()).unwrap()
    } else {
        ResourceAttributeInputs::from_report(resource, &values, judged.report())
    };
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    with_session_for_test(&catalog, |session| {
        let findings = {
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
        assert!(
            findings
                .errors()
                .chain(findings.symbolic_errors().map(|(_, _, e)| e))
                .any(|e| e.to_string().contains("expected String, got Int")),
            "admission must judge its product's invalid value: {findings:?}"
        );
    });
}

#[test]
fn reported_inputs_are_classified_from_their_own_values() {
    rejects_invalid_product_value(false);
}

#[test]
fn projected_inputs_are_classified_from_their_own_values() {
    rejects_invalid_product_value(true);
}
