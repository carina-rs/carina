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
fn issue_4516_deferred_for_child_rows_keep_element_identity() {
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
            assert_eq!(child.id.identity_str(), Some(address.as_str()));
            assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Secret));
            let mut graph = graph(&file).clone();
            graph.insert_child(child).unwrap();
            assert_eq!(graph.attributes(&child.id), Some(row.attributes()));
        }
    }
}

#[test]
fn issue_4516_deferred_for_pending_directory_carries_unavailable() {
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
fn issue_4516_deferred_for_without_admission_still_expands() {
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
fn issue_4516_deferred_for_rejects_unrelated_evidence_and_children() {
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
fn issue_4516_deferred_for_duplicate_child_insert_is_typed() {
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
fn issue_4516_insert_child_rejects_resource_not_produced_by_deferred_for() {
    let (_directory, file, schemas, context) =
        directory("let ordinary = mock.test.Resource { comment = 'public' }\n");
    let file = resolved(file, &schemas, &context);
    let resource = file.resources.first().unwrap();
    assert!(resource.child_secrecy().is_none());
    let mut graph = graph(&file).clone();
    let error = graph.insert_child(resource).unwrap_err();
    assert_eq!(error, ChildEvidenceInsertError::NotDeferredChild);
    assert_eq!(
        error.to_string(),
        "resource was not produced by deferred-for expansion"
    );
}

#[test]
fn issue_4516_deferred_for_without_children_leaves_attached_graph_untouched() {
    for (main, iterable) in [("", None), (ROOT_LOOP, None), (ROOT_LOOP, Some(list(&[])))] {
        let (_directory, file, schemas, context) = directory(main);
        let mut file = resolved(file, &schemas, &context);
        let before = file.authored_inputs.as_ref().unwrap().clone();
        let resources_before = file.resources.clone();
        if let Some(iterable) = iterable {
            expand(&mut file, iterable, &context);
            assert!(file.deferred_for_expressions().is_empty());
        } else {
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                file.expand_deferred_for_expressions(
                    &crate::binding_index::IterableBindings::default(),
                    session,
                )
            })
            .unwrap();
            assert_eq!(
                file.deferred_for_expressions().len(),
                usize::from(!main.is_empty())
            );
        }
        assert_eq!(file.resources, resources_before);
        let after = file.authored_inputs.as_ref().unwrap();
        assert!(std::sync::Arc::ptr_eq(&before, after));
        assert!(std::ptr::eq(
            before.validated_secrecy().unwrap(),
            after.validated_secrecy().unwrap()
        ));
    }
}

#[test]
fn issue_4516_without_authored_inputs_keeps_templates_unavailable_and_reader_pending() {
    let (_directory, mut file, schemas, context) = directory(ROOT_LOOP);
    file.authored_inputs = None;
    assert!(file.authored_inputs().is_err());
    let buffer = crate::config_loader::load_buffer_configuration(&file, &schemas, &context);
    assert!(buffer.judged().secrecy().admitted().is_some());
    let file = buffer.parsed().clone();
    assert!(file.authored_inputs().is_err());
    let resolved = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::validation::resolve_bare_binding_uses_in_session(
            file,
            &schemas,
            session,
            &crate::module_resolver::ResolvedModuleSignatures::default(),
        )
    })
    .unwrap();
    assert!(resolved.as_file().authored_inputs().is_err());
    assert!(matches!(
        resolved.validated_secrecy(),
        Err(crate::secrecy::PendingSecrecy)
    ));
    assert!(matches!(
        resolved.deferred_for_expressions()[0].secrecy(),
        TemplateSecrecy::Unavailable(TemplateEvidenceUnavailable::ConfigurationNotAdmitted)
    ));
    let mut file = resolved.into_file();
    expand(&mut file, list(&["a", "b"]), &context);
    assert!(file.deferred_for_expressions().is_empty());
    assert_eq!(file.resources.len(), 3);
    for child in file.resources.iter().filter(|resource| {
        resource
            .id
            .identity_str()
            .is_some_and(|id| id.starts_with("children["))
    }) {
        assert!(matches!(
            child.child_secrecy(),
            Some(ChildSecrecy::Unavailable(
                TemplateEvidenceUnavailable::ConfigurationNotAdmitted
            ))
        ));
    }
}

#[test]
fn issue_4516_child_row_rejects_transplanted_authored_slots() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let child = children(&file.deferred_for_expressions()[0], list(&["a"]), &context).remove(0);
    let mut other = Resource::with_provider("mock", "test.Resource", "other", None);
    other.authored_slots = child.authored_slots.clone();
    let mut graph = graph(&file).clone();
    let observed = (other.child_secrecy().is_none(), graph.insert_child(&other));
    assert_eq!(
        observed,
        (true, Err(ChildEvidenceInsertError::ChildDeclaration))
    );
    assert!(graph.attributes(&other.id).is_none());
}

#[test]
fn issue_4516_child_row_rejects_mutated_provider() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let mut child = children(&file.deferred_for_expressions()[0], list(&["a"]), &context).remove(0);
    child.id.provider = "foreign".into();
    let mut graph = graph(&file).clone();
    let observed = (child.child_secrecy().is_none(), graph.insert_child(&child));
    assert_eq!(
        observed,
        (true, Err(ChildEvidenceInsertError::ChildDeclaration))
    );
    assert!(graph.attributes(&child.id).is_none());
}

#[test]
fn issue_4516_child_row_rejects_mutated_provider_instance_type_and_element() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let original = children(&file.deferred_for_expressions()[0], list(&["a"]), &context).remove(0);
    for mutation in ["provider instance", "resource type", "element"] {
        let mut child = original.clone();
        match mutation {
            "provider instance" => child.id.provider_instance = Some("foreign".into()),
            "resource type" => child.id.resource_type = "test.Foreign".into(),
            "element" => {
                child.id.set_identity("children[1]".into());
                child.binding =
                    Some(crate::resource::RootScope::new().resolve("children[1]".into()));
            }
            _ => unreachable!(),
        }
        let mut graph = graph(&file).clone();
        assert_eq!(
            (child.child_secrecy().is_none(), graph.insert_child(&child)),
            (true, Err(ChildEvidenceInsertError::ChildDeclaration)),
            "{mutation}",
        );
    }
}

#[test]
fn issue_4516_child_row_rejects_anonymous_element_after_identifier_computation() {
    let (_directory, file, schemas, context) = directory(
        "for item in upstream.items { mock.test.Child { comment = secret('canary') identity_input = item } }\n",
    );
    let file = resolved(file, &schemas, &context);
    let mut children = children(
        &file.deferred_for_expressions()[0],
        list(&["a", "b"]),
        &context,
    );
    crate::identifier::compute_anonymous_identifiers_for_test(
        &mut children,
        &file.providers,
        &schemas,
        &|_| Vec::new(),
    )
    .unwrap();
    children[1].authored_slots = children[0].authored_slots.clone();
    let child = &children[1];
    let mut graph = graph(&file).clone();
    assert_eq!(
        (child.child_secrecy().is_none(), graph.insert_child(child)),
        (true, Err(ChildEvidenceInsertError::ChildDeclaration)),
    );
}

#[tokio::test]
async fn issue_4516_deferred_for_normalization_retains_checked_child() {
    for bound in [true, false] {
        let prefix = if bound { "let children = " } else { "" };
        let (_directory, file, schemas, context) = directory(&format!(
            "{prefix}for item in upstream.items {{ mock.test.Child {{ comment = secret('canary') identity_input = item }} }}\n",
        ));
        let file = resolved(file, &schemas, &context);
        let mut children = children(&file.deferred_for_expressions()[0], list(&["a"]), &context);
        if !bound {
            crate::identifier::compute_anonymous_identifiers_for_test(
                &mut children,
                &file.providers,
                &schemas,
                &|_| Vec::new(),
            )
            .unwrap();
        }
        let normalized = crate::executor::normalized::apply_host_desired_normalization(
            children.remove(0),
            &file.providers,
            &crate::provider::NoopNormalizer,
            &schemas,
        )
        .await
        .unwrap();
        let child = normalized.as_resource();
        let Some(ChildSecrecy::Row(row)) = child.child_secrecy() else {
            panic!("normalization must retain the checked child's row");
        };
        assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Secret));
        graph(&file).clone().insert_child(child).unwrap();
    }
}

#[test]
fn issue_4516_unavailable_child_errors_use_plain_words() {
    for (reason, expected) in [
        (
            TemplateEvidenceUnavailable::ConfigurationNotAdmitted,
            "deferred child secrecy is unavailable: configuration was not admitted",
        ),
        (
            TemplateEvidenceUnavailable::SavedPlan,
            "deferred child secrecy is unavailable: template was decoded from a saved plan",
        ),
    ] {
        assert_eq!(
            ChildEvidenceInsertError::Unavailable(reason).to_string(),
            expected
        );
    }
}

#[test]
fn issue_4516_deferred_for_evidence_ignores_child_and_provider_values() {
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
fn issue_4516_deferred_for_distinguishes_module_instances() {
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
fn issue_4516_deferred_for_admitted_graph_requires_a_template_row() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let (_other_directory, other, other_schemas, other_context) = directory(ROOT_LOOP);
    let other = other.into_expanded(&other_schemas, &other_context).unwrap();
    assert!(matches!(
        graph(&file).template_evidence(&other.deferred_for_expressions()[0]),
        Err(TemplateAdmissionError::MissingTemplateEvidence { .. })
    ));
}

#[test]
fn issue_4516_static_resource_has_no_child_judgement() {
    let (_directory, file, schemas, context) =
        directory("let ordinary = mock.test.Resource { comment = 'public' }\n");
    let file = resolved(file, &schemas, &context);
    assert!(file.deferred_for_expressions().is_empty());
    for resource in &file.resources {
        assert!(resource.child_secrecy().is_none());
    }
}

#[test]
fn issue_4516_static_for_child_has_no_child_judgement() {
    let (_directory, file, schemas, context) = directory(
        "let children = for item in ['a', 'b'] { mock.test.Resource { comment = item } }\n",
    );
    let file = resolved(file, &schemas, &context);
    assert!(file.deferred_for_expressions().is_empty());
    let children = file
        .resources
        .iter()
        .filter(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|binding| binding.as_str().starts_with("children"))
        })
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2);
    for child in children {
        assert!(child.child_secrecy().is_none());
    }
}

#[test]
fn issue_4516_deferred_child_has_judgement_and_reinstance_clears_it() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let mut file = resolved(file, &schemas, &context);
    expand(&mut file, list(&["a"]), &context);
    let child = file
        .resources
        .iter()
        .find(|resource| resource.id.identity_str() == Some("children[0]"))
        .unwrap();
    assert!(matches!(child.child_secrecy(), Some(ChildSecrecy::Row(_))));
    assert!(child.instantiate().child_secrecy().is_none());
}

#[test]
fn issue_4516_admitted_file_rejects_deferred_child_without_template_evidence() {
    for append in [false, true] {
        // An admitted file rejects a deferred child whose template carries no evidence.
        let (_directory, file, schemas, context) = directory("");
        let (_raw_directory, raw, _, _) = directory(ROOT_LOOP);
        let unadmitted_file = raw.into_expanded(&schemas, &context).unwrap();
        let mut file = resolve_owner(file, &schemas, &context).into_file();
        assert!(file.authored_inputs().unwrap().validated_secrecy().is_ok());
        let template = &unadmitted_file.deferred_for_expressions()[0];
        let unadmitted = crate::parser::DeferredForExpression::new(
            template.file().map(str::to_owned),
            template.line(),
            template.header.clone(),
            template.target().clone(),
            template.iterable.clone(),
            template.binding.clone(),
            template.template_resource().as_resource().clone(),
        );
        assert!(matches!(
            unadmitted.secrecy(),
            TemplateSecrecy::Unavailable(TemplateEvidenceUnavailable::ConfigurationNotAdmitted)
        ));
        if append {
            file.deferred_for_expressions_mut()
                .append_finalized(unadmitted_file.into_deferred_for_expressions());
        } else {
            file.replace_deferred_for_expressions(unadmitted_file.into_deferred_for_expressions());
            *file
                .deferred_for_expressions_mut()
                .iter_mut()
                .next()
                .unwrap() = unadmitted;
        }
        assert!(file.authored_inputs().unwrap().validated_secrecy().is_ok());
        let bindings = crate::binding_index::IterableBindings::from_upstream_only(
            crate::upstream_exports::remote_bindings_for_test(HashMap::from([(
                "upstream".into(),
                HashMap::from([("items".into(), list(&["a"]))]),
            )])),
        );
        let result =
            crate::validation::deferred_types::with_session_for_test(&context, |session| {
                file.expand_deferred_for_expressions(&bindings, session)
            });
        assert!(matches!(
            result,
            Err(crate::parser::DeferredForExpansionError::ChildSecrecy(
                ChildEvidenceInsertError::Unavailable(
                    TemplateEvidenceUnavailable::ConfigurationNotAdmitted
                )
            ))
        ));
        assert_eq!(file.resources.len(), 1);
        assert_eq!(file.deferred_for_expressions().len(), 1);
    }
}
