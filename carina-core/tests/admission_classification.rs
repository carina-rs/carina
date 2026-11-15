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
    let bindings = judged.parsed().binding_index(&catalog);
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

#[test]
fn raw_file_validation_prefix_matches_value_only_admission() {
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let parsed = parse_file_and_resolve(
        "mock.Test { name_prefix = 'x' }",
        std::path::Path::new("main.crn"),
        &context,
    )
    .unwrap();
    let expected = carina_core::test_utils::admit_resource_attributes(
        schemas
            .get("mock", "Test", carina_core::schema::SchemaKind::Resource)
            .unwrap(),
        &parsed.resources[0].attributes,
        &|_| true,
        &Default::default(),
    );
    assert!(
        expected.is_ok(),
        "the value-only path recognizes this companion: {expected:?}"
    );
    let actual = carina_core::validation::validate_resources(
        &parsed,
        &schemas,
        &std::collections::HashSet::from(["mock".into()]),
        &context,
        &mut Vec::new(),
    );
    assert!(
        actual.is_ok(),
        "raw-file validation must agree with the value-only path: {actual:?}"
    );
}

#[test]
fn raw_file_validation_retains_typed_prefix_failures() {
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    for (source, message) in [
        (
            "mock.Test { name_prefix = '' }",
            "'name_prefix' cannot be empty",
        ),
        (
            "mock.Test { name = 'a'\nname_prefix = 'x' }",
            "cannot specify both 'name_prefix' and 'name'",
        ),
    ] {
        let parsed =
            parse_file_and_resolve(source, std::path::Path::new("main.crn"), &context).unwrap();
        let findings = carina_core::validation::validate_resources(
            &parsed,
            &schemas,
            &std::collections::HashSet::from(["mock".into()]),
            &context,
            &mut Vec::new(),
        )
        .unwrap_err();
        let errors: Vec<_> = findings
            .iter()
            .filter_map(|finding| match finding {
                carina_core::validation::PresentedFinding::Ordinary(
                    carina_core::validation::OrdinaryFinding::Resource(finding),
                ) => finding.type_error(),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "{findings:?}");
        assert!(
            matches!(
                errors[0],
                carina_core::schema::TypeError::AttributePrefix(_)
            ),
            "{errors:?}"
        );
        assert!(errors[0].to_string().contains(message), "{errors:?}");
    }
}

#[test]
fn raw_file_validation_reports_prefix_and_other_owner_errors() {
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(AttributeSchema::required("name", AttributeType::string()).build())
            .validate()
            .unwrap(),
    );
    let parsed = parse_file_and_resolve(
        "let bad_prefix = mock.Test { name_prefix = '' }\nlet other = mock.Test { name = 7 }",
        std::path::Path::new("main.crn"),
        &context,
    )
    .unwrap();
    let findings = carina_core::validation::validate_resources(
        &parsed,
        &schemas,
        &std::collections::HashSet::from(["mock".into()]),
        &context,
        &mut Vec::new(),
    )
    .unwrap_err();
    let messages = findings
        .iter()
        .map(|finding| finding.message())
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 2, "{findings:?}");
    assert!(
        messages
            .iter()
            .any(|message| message.contains("'name_prefix' cannot be empty")),
        "{messages:?}"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("expected String, got Int")),
        "{messages:?}"
    );
}
