use super::*;

#[test]
fn round1_seam_materialization_reports_unmatched_attribute_names() {
    let parsed = crate::parser::parse_and_resolve("mock.Test { input = 'value' }").unwrap();
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let tree = crate::parser::AnalysisTree::try_from_file(configuration.judged().file()).unwrap();
    let mut inputs = crate::validation::admission::attributes::ResourceAttributeInputs::new(
        tree.resources().next().unwrap(),
    );
    let outcome = inputs.materialize(&indexmap::IndexMap::from([(
        "unmatched".into(),
        Value::Concrete(crate::resource::ConcreteValue::Int(1)),
    )]));
    assert!(
        format!("{outcome:?}").contains("unmatched"),
        "unmatched names must be reported: {outcome:?}"
    );
}

#[test]
fn seam_known_if_preserves_the_single_main_local_obligation() {
    use crate::schema::{AttributeSchema, AttributeType as T, ResourceSchema, TypeIdentity};
    use crate::validation::admission::{Candidate, admit};
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    let identity = Some(TypeIdentity::bare("Id"));
    schemas.insert(
        "mock",
        ResourceSchema::new("Source")
            .attribute(
                AttributeSchema::optional(
                    "output",
                    T::refined_string(identity.clone(), None, Some((None, Some(64))), None),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        ResourceSchema::new("Test")
            .attribute(
                AttributeSchema::optional(
                    "input",
                    T::refined_string(identity, None, Some((None, Some(36))), None),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed = crate::parser::parse_and_resolve("let source = mock.Source {}\nmock.Test { input = if true { source.output } else { source.output } }").unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let resource = &judged.file().resources[1];
    let slot = resource.authored_slots.get("input").unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    let destination = schemas
        .resource_attribute_destination(&resource.id, "input", &catalog)
        .unwrap();
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let proof = slot
            .projection()
            .unwrap()
            .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
            .unwrap()
            .into_classification();
        let (candidate, context) =
            Candidate::validation_slot(slot, proof, Some(&bindings), session).unwrap();
        let pending = admit(&destination, candidate, &context)
            .unwrap()
            .ready()
            .unwrap_err();
        assert_eq!(pending.local_obligation().unwrap().paths().len(), 1);
    });
}

#[test]
fn review_seam_producer_materialization_keeps_a_real_projection_pairing() {
    use crate::validation::admission::attributes::{
        ResourceAttributeInputs, resource_attribute_admissions,
    };
    let catalog = ProviderContext::new(crate::EnvironmentSnapshot::empty());
    let mut schemas = SchemaRegistry::new();
    schemas.insert(
        "mock",
        crate::schema::ResourceSchema::new("Test")
            .attribute(
                crate::schema::AttributeSchema::optional(
                    "input",
                    crate::schema::AttributeType::string(),
                )
                .build(),
            )
            .validate()
            .unwrap(),
    );
    let parsed =
        crate::parser::parse_and_resolve("let one = mock.Test { input = 'before' }").unwrap();
    let configuration =
        crate::config_loader::load_buffer_configuration(&parsed, &schemas, &catalog);
    let judged = configuration.judged();
    let tree = crate::parser::AnalysisTree::try_from_file(judged.file()).unwrap();
    let resource = tree.resources().next().unwrap();
    let source = resource.authored_slot("input").unwrap();
    let mut inputs = ResourceAttributeInputs::new(resource);
    inputs
        .materialize(&indexmap::IndexMap::from([(
            "input".into(),
            Value::Concrete(crate::resource::ConcreteValue::String("after".into())),
        )]))
        .unwrap();
    let bindings =
        crate::binding_index::BindingIndex::from_parsed(judged.file(), &schemas, &catalog);
    crate::validation::deferred_types::with_session_for_test(&catalog, |session| {
        let mut coverage = crate::validation::admission::WalkCoverage::default();
        let findings = {
            let outcome = resource_attribute_admissions(
                resource,
                judged.file(),
                resource.schema(&schemas).unwrap(),
                resource.quoted_string_attrs(),
                inputs,
                crate::validation::admission::attributes::ValidationEvaluation::None,
                &bindings,
                session,
                &schemas,
                &mut coverage,
            );
            assert!(outcome.classification_diagnostics.0.is_empty());
            outcome.values
        };
        assert_eq!(findings.errors().count(), 0, "{findings:?}");
        let usage = source.cursor().unwrap();
        assert_ne!(
            coverage
                .current_generation(usage.occurrence(), usage.instance())
                .unwrap(),
            source.generation()
        );
    });
}
