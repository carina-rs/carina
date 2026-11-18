use super::*;
use crate::resource::{AccessMissPolicy, ForLoopElement, Resource};
use crate::secrecy::{
    ChildEvidenceInsertError, ChildSecrecy, TemplateAdmissionError, TemplateEvidenceMismatch,
    TemplateEvidenceUnavailable, TemplateSecrecy,
};

fn children(
    template: &ExpandedDeferredForExpression,
    iterable: Value,
    context: &ProviderContext,
) -> Vec<Resource> {
    use crate::resource::knowledge::{Classification, KnowledgeContext, PlanIterable};
    let Classification::Known(known) = iterable
        .knowledge(&KnowledgeContext::validation())
        .unwrap()
        .into_classification()
    else {
        panic!("fixture iterable is known");
    };
    crate::validation::deferred_types::with_session_for_test(context, |session| {
        crate::parser::expand_deferred_children(
            template,
            PlanIterable::from_known(known).unwrap(),
            AccessMissPolicy::Error,
            session,
        )
        .unwrap()
    })
}

#[test]
fn issue_4516_t3_deferred_for_child_rows_keep_element_identity() {
    for (main, both, only_b, first_element, second_element) in [
        (
            "let children = for key, item in upstream.items { mock.test.Resource { comment = secret('canary') name = item } }\n",
            map(&["a", "b"]),
            map(&["b"]),
            ForLoopElement::Key("b".into()),
            ForLoopElement::Key("b".into()),
        ),
        (
            ROOT_LOOP,
            list(&["a", "b"]),
            list(&["b"]),
            ForLoopElement::Index(1),
            ForLoopElement::Index(0),
        ),
    ] {
        let (_directory, file, schemas, context) = directory(main);
        let file = resolved(file, &schemas, &context);
        let template = &file.deferred_for_expressions()[0];
        for (iterable, element) in [(both, first_element), (only_b, second_element)] {
            let address = element.address("children");
            let children = children(template, iterable, &context);
            let child = children
                .iter()
                .find(|child| child.id.identity_str() == Some(address.as_str()))
                .unwrap();
            let Some(ChildSecrecy::Row(row)) = child.child_secrecy() else {
                panic!("admitted child's row");
            };
            assert_eq!(row.element(), &element);
            assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Secret));
        }
    }
}

#[test]
fn issue_4516_t4_deferred_for_pending_directory_carries_unavailable() {
    // The missing NoSchema schema is the pending cause.
    let (_directory, file, schemas, context) = directory(&format!(
        "{ROOT_LOOP}\nlet missing = mock.test.NoSchema {{}}\n"
    ));
    let report = report(&file, &schemas, &context);
    assert!(report.is_pending());
    assert!(report.admitted().is_none());
    let mut file = resolved(file, &schemas, &context);
    let template = &file.deferred_for_expressions()[0];
    assert!(matches!(
        template.secrecy(),
        TemplateSecrecy::Unavailable(TemplateEvidenceUnavailable::ConfigurationNotAdmitted)
    ));
    for child in children(template, list(&["a", "b"]), &context) {
        assert!(matches!(
            child.child_secrecy(),
            Some(ChildSecrecy::Unavailable(
                TemplateEvidenceUnavailable::ConfigurationNotAdmitted
            ))
        ));
    }
    expand(&mut file, list(&["a", "b"]), &context);
    assert!(file.deferred_for_expressions().is_empty());
    assert!(file.authored_inputs().unwrap().validated_secrecy().is_err());
}

#[test]
fn issue_4516_t4_deferred_for_without_admission_still_expands() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let mut file = file.into_expanded(&schemas, &context).unwrap();
    let template = &file.deferred_for_expressions()[0];
    assert!(matches!(
        template.secrecy(),
        TemplateSecrecy::Unavailable(TemplateEvidenceUnavailable::ConfigurationNotAdmitted)
    ));
    for child in children(template, list(&["a", "b"]), &context) {
        assert!(matches!(
            child.child_secrecy(),
            Some(ChildSecrecy::Unavailable(
                TemplateEvidenceUnavailable::ConfigurationNotAdmitted
            ))
        ));
    }
    expand(&mut file, list(&["a", "b"]), &context);
    assert_eq!(file.resources.len(), 3);
}

#[test]
fn issue_4516_t5_deferred_for_rejects_unrelated_evidence_and_children() {
    let (_directory, file, schemas, context) = directory(&format!(
        "{ROOT_LOOP}\nlet others = for item in upstream.items {{ mock.test.Resource {{ comment = secret('different') name = item }} }}\n"
    ));
    let file = resolved(file, &schemas, &context);
    let first = &file.deferred_for_expressions()[0];
    let second = &file.deferred_for_expressions()[1];
    let TemplateSecrecy::Derived(evidence) = first.secrecy() else {
        panic!("admitted template must carry evidence");
    };
    let child = children(first, list(&["a"]), &context).remove(0);
    assert!(matches!(
        evidence.instantiate_child(second, &ForLoopElement::Index(0), &child),
        Err(TemplateEvidenceMismatch::TemplateDeclaration)
    ));
    let other_child = children(second, list(&["a"]), &context).remove(0);
    assert!(matches!(
        evidence.instantiate_child(first, &ForLoopElement::Index(0), &other_child),
        Err(TemplateEvidenceMismatch::ChildDeclaration)
    ));
    let mut wrong_provider = child.clone();
    wrong_provider.id.provider = "other".into();
    assert!(matches!(
        evidence.instantiate_child(first, &ForLoopElement::Index(0), &wrong_provider),
        Err(TemplateEvidenceMismatch::Provider)
    ));
    let mut wrong_type = child.clone();
    wrong_type.id.resource_type = "test.Other".into();
    assert!(matches!(
        evidence.instantiate_child(first, &ForLoopElement::Index(0), &wrong_type),
        Err(TemplateEvidenceMismatch::ResourceType)
    ));
    assert!(matches!(
        evidence.instantiate_child(first, &ForLoopElement::Index(1), &child),
        Err(TemplateEvidenceMismatch::Element)
    ));
}

#[test]
fn issue_4516_t5_deferred_for_duplicate_child_insert_is_typed() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let child = children(&file.deferred_for_expressions()[0], list(&["a"]), &context).remove(0);
    let mut graph = graph(&file).clone();
    graph.insert_child(&child).unwrap();
    assert!(
        matches!(graph.insert_child(&child), Err(ChildEvidenceInsertError::DuplicateChild { id }) if id == child.id)
    );
    assert_eq!(
        graph.attributes(&child.id).unwrap()["comment"],
        Evidence::Leaf(Secrecy::Secret)
    );
}

#[test]
fn issue_4516_t5_deferred_for_evidence_ignores_child_and_provider_values() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let mut child = children(&file.deferred_for_expressions()[0], list(&["a"]), &context).remove(0);
    child.attributes.insert(
        "comment".into(),
        Value::Concrete(ConcreteValue::String("evaluated public value".into())),
    );
    child.attributes.insert(
        "identifier".into(),
        Value::Concrete(ConcreteValue::String("provider populated".into())),
    );
    let mut graph = graph(&file).clone();
    graph.insert_child(&child).unwrap();
    let row = graph.attributes(&child.id).unwrap();
    assert_eq!(row["comment"], Evidence::Leaf(Secrecy::Secret));
    assert!(!row.contains_key("identifier"));
}

#[test]
fn issue_4516_t5_deferred_for_distinguishes_module_instances() {
    let (_directory, file, schemas, context) = fixture(
        "let child = use { source = './child' }\nlet first = child { items = upstream.items }\nlet second = child { items = upstream.items }\n",
        Some(
            "arguments { items: list(String) }\nlet children = for item in items { mock.test.Resource { comment = secret('canary') name = item } }\n",
        ),
    );
    let file = resolved(file, &schemas, &context);
    let first = &file.deferred_for_expressions()[0];
    let second = &file.deferred_for_expressions()[1];
    let TemplateSecrecy::Derived(evidence) = first.secrecy() else {
        panic!("admitted template evidence");
    };
    let child = children(second, list(&["a"]), &context).remove(0);
    assert!(matches!(
        evidence.instantiate_child(second, &ForLoopElement::Index(0), &child),
        Err(TemplateEvidenceMismatch::TemplateDeclaration)
    ));
    assert!(matches!(
        evidence.instantiate_child(first, &ForLoopElement::Index(0), &child),
        Err(TemplateEvidenceMismatch::ChildDeclaration)
    ));
}

#[test]
fn issue_4516_t5_deferred_for_admitted_graph_requires_a_template_row() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let (_other_directory, other, other_schemas, other_context) = directory(ROOT_LOOP);
    let other = other.into_expanded(&other_schemas, &other_context).unwrap();
    assert!(matches!(
        graph(&file).template_evidence(&other.deferred_for_expressions()[0]),
        Err(TemplateAdmissionError::MissingTemplateEvidence { .. })
    ));
}
