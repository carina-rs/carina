use carina_core::binding_index::BindingIndex;
use carina_core::parser::{ProviderContext, parse_file_and_resolve};
use carina_core::resource::{ConcreteValue, Value};
use carina_core::schema::{AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry};
use carina_core::test_utils::load_canonical_buffer_configuration as load_buffer_configuration;
use carina_core::validation::admission::attributes::{
    ResourceAdmissionRequest, ResourceAttributeInputs,
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
    let parsed = parse_file_and_resolve(
        if projected {
            "mock.Test { name_prefix = 'valid-' }"
        } else {
            "mock.Test { name = 'valid' }"
        },
        std::path::Path::new("admission.crn"),
        &catalog,
    )
    .unwrap();
    let configuration = load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = judged.analysis_tree().unwrap();
    let resource = tree.resources().next().unwrap();
    let mut values = resource.attributes().into_owned();
    values.insert("name".into(), Value::Concrete(ConcreteValue::Int(7)));
    let values = resource.with_values(&values).unwrap();
    let inputs = if projected {
        assert!(resource.authored_slot("name").is_err());
        ResourceAttributeInputs::from_resolved(resource, &values, judged.report()).unwrap()
    } else {
        ResourceAttributeInputs::from_report(resource, &values, judged.report())
    };
    let bindings = BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    with_session_for_test(&catalog, |session| {
        let findings = {
            let request = ResourceAdmissionRequest::new(
                resource,
                resource.quoted_string_attrs(),
                inputs,
                carina_core::validation::admission::attributes::ValidationEvaluation::None,
                &bindings,
                session,
            )
            .unwrap();
            let mut validation = carina_core::validation::ConversionValidation::<
                std::convert::Infallible,
            >::new(judged.report());
            let outcome = validation.admit_resource(request);
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
