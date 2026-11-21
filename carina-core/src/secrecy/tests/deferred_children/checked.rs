use super::*;
use crate::resource::{AccessMissPolicy, ForLoopElement, Resource};
use crate::secrecy::{
    ChildEvidenceInsertError, ChildSecrecyRead, TemplateAdmissionError, TemplateEvidenceMismatch,
    TemplateEvidenceUnavailable, TemplateSecrecy,
};

fn admitted_member(
    template: &ExpandedDeferredForExpression,
    key: &str,
) -> crate::resource::AdmittedForMember<
    crate::resource::NoPlaceholder,
    crate::resource::AbsoluteBindingAddress,
> {
    let crate::parser::DeferredForTarget::Bound { target, .. } = template.target() else {
        panic!("fixture is bound");
    };
    let iterable = list(&[key]);
    let classification = iterable
        .knowledge(&crate::resource::knowledge::KnowledgeContext::validation())
        .unwrap()
        .into_classification();
    let crate::resource::ForAddressAdmission::Admitted(mut members) =
        crate::resource::admit_bound_for_addresses(
            &template.target().binding_pattern(),
            target,
            crate::resource::knowledge::ClassifiedRef::from(&classification),
            crate::resource::ForAddressMoment::Plan,
        )
        .unwrap()
    else {
        panic!("fixture keys are admitted");
    };
    members.remove(0)
}

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
        crate::parser::expand_deferred_children_for_test(
            template,
            PlanIterable::from_known(known).unwrap(),
            AccessMissPolicy::Error,
            session,
        )
        .unwrap()
    })
}

#[test]
fn issue_4516_anonymous_module_remapped_template_keeps_owned_child_evidence() {
    let (_directory, file, schemas, context) = fixture(
        "let child = use { source = './child' }\nchild { items = upstream.items }\n",
        Some(
            "arguments { items: list(String) }\nlet children = for item in items { mock.test.Resource { comment = secret('canary') name = item } }\n",
        ),
    );
    let mut file = resolve_owner(file, &schemas, &context).into_file();
    let template = &file.deferred_for_expressions()[0];
    let original_id = template.template_resource_id().clone();
    let TemplateSecrecy::Derived(evidence) = template.secrecy() else {
        panic!("admitted template must carry evidence");
    };
    let evidence = std::sync::Arc::clone(evidence);
    let (prefix, _) = original_id.identity_str().unwrap().split_once('.').unwrap();
    let (module, hash) = prefix.rsplit_once('_').unwrap();
    let hash = crate::identifier::SimHash::parse_16_hex(hash).unwrap();
    let state_prefix = format!("{module}_{:016x}", hash.with_flipped_mask_for_test(1));

    // The CLI reconciles state addresses after admission, before expansion.
    crate::module_resolver::reconcile_anonymous_module_instances(
        &mut file,
        &|_, _| vec![format!("{state_prefix}.module_unrelated")],
        &crate::identifier::StateBlockClaims::empty(),
    );
    let remapped = file.deferred_for_expressions()[0].clone();
    assert_ne!(remapped.template_resource_id(), &original_id);
    assert_eq!(
        remapped.template_resource_id().identity_str(),
        Some(format!("{state_prefix}.children[?]").as_str())
    );
    assert!(graph(&file).templates.contains_key(&original_id));
    assert!(
        !graph(&file)
            .templates
            .contains_key(remapped.template_resource_id())
    );
    let TemplateSecrecy::Derived(remapped_evidence) = remapped.secrecy() else {
        panic!("remapping must retain admitted evidence");
    };
    assert!(std::sync::Arc::ptr_eq(&evidence, remapped_evidence));

    let (selected, retained) = file.take_deferred_for_expressions().partition(|_| true);
    file.replace_deferred_for_expressions(selected);
    expand(&mut file, list(&["a", "b"]), &context);
    file.deferred_for_expressions_mut()
        .append_finalized(retained);
    assert!(file.deferred_for_expressions().is_empty());
    assert_child_row(&file, &format!("{state_prefix}.children.a"));
    assert_child_row(&file, &format!("{state_prefix}.children.b"));
    assert!(std::sync::Arc::ptr_eq(
        &graph(&file).template_evidence(&remapped).unwrap(),
        &evidence
    ));
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
            "let children = for (i, item) in upstream.items { mock.test.Resource { comment = secret('canary') name = item } }\n",
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
            let row = match child.child_secrecy() {
                ChildSecrecyRead::Row(row) => row,
                unexpected @ (ChildSecrecyRead::NotLoopChild
                | ChildSecrecyRead::Unavailable(_)
                | ChildSecrecyRead::RowMismatch) => {
                    panic!("expected admitted child's row, got {unexpected:?}")
                }
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
fn for_value_keys_deferred_for_child_rows_keep_element_identity() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let children = children(
        &file.deferred_for_expressions()[0],
        list(&["a", "b"]),
        &context,
    );
    assert_eq!(children.len(), 2);
    let mut graph = graph(&file).clone();
    for (address, other_address) in [("children.a", "children.b"), ("children.b", "children.a")] {
        let child = children
            .iter()
            .find(|child| child.id.identity_str() == Some(address))
            .unwrap();
        let other = children
            .iter()
            .find(|child| child.id.identity_str() == Some(other_address))
            .unwrap();
        let row = match child.child_secrecy() {
            ChildSecrecyRead::Row(row) => row,
            unexpected @ (ChildSecrecyRead::NotLoopChild
            | ChildSecrecyRead::Unavailable(_)
            | ChildSecrecyRead::RowMismatch) => {
                panic!("expected admitted child's row, got {unexpected:?}")
            }
        };
        assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Secret));
        assert_eq!(row.attributes()["name"], Evidence::Leaf(Secrecy::Public));
        assert!(row.matches_resource(child));
        assert!(!row.matches_resource(other));
        graph.insert_child(child).unwrap();
        assert_eq!(graph.attributes(&child.id), Some(row.attributes()));

        let mut wrong_element = child.clone();
        wrong_element.id.set_identity(other_address.into());
        wrong_element.binding = other.binding.clone();
        assert!(!row.matches_resource(&wrong_element));
        assert!(matches!(
            wrong_element.child_secrecy(),
            ChildSecrecyRead::RowMismatch
        ));
        assert_eq!(
            graph.insert_child(&wrong_element),
            Err(ChildEvidenceInsertError::ChildDeclaration)
        );
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
        match child.child_secrecy() {
            ChildSecrecyRead::Unavailable(reason) => assert_eq!(
                reason,
                TemplateEvidenceUnavailable::ConfigurationNotAdmitted
            ),
            unexpected @ (ChildSecrecyRead::NotLoopChild
            | ChildSecrecyRead::Row(_)
            | ChildSecrecyRead::RowMismatch) => {
                panic!("expected unavailable child evidence, got {unexpected:?}")
            }
        }
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
        match child.child_secrecy() {
            ChildSecrecyRead::Unavailable(reason) => assert_eq!(
                reason,
                TemplateEvidenceUnavailable::ConfigurationNotAdmitted
            ),
            unexpected @ (ChildSecrecyRead::NotLoopChild
            | ChildSecrecyRead::Row(_)
            | ChildSecrecyRead::RowMismatch) => {
                panic!("expected unavailable child evidence, got {unexpected:?}")
            }
        }
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
    let member = admitted_member(first, "a");
    let wrong_member = admitted_member(first, "b");
    assert!(matches!(
        evidence.instantiate_child(
            second,
            &crate::parser::ExpandedForChild::Bound(&member),
            &child
        ),
        Err(TemplateEvidenceMismatch::TemplateDeclaration)
    ));
    let other_child = children(second, list(&["a"]), &context).remove(0);
    assert!(matches!(
        evidence.instantiate_child(
            first,
            &crate::parser::ExpandedForChild::Bound(&member),
            &other_child
        ),
        Err(TemplateEvidenceMismatch::ChildDeclaration)
    ));
    let mut wrong_provider = child.clone();
    wrong_provider.id.provider = "other".into();
    assert!(matches!(
        evidence.instantiate_child(
            first,
            &crate::parser::ExpandedForChild::Bound(&member),
            &wrong_provider
        ),
        Err(TemplateEvidenceMismatch::Provider)
    ));
    let mut wrong_type = child.clone();
    wrong_type.id.resource_type = "test.Other".into();
    assert!(matches!(
        evidence.instantiate_child(
            first,
            &crate::parser::ExpandedForChild::Bound(&member),
            &wrong_type
        ),
        Err(TemplateEvidenceMismatch::ResourceType)
    ));
    assert!(matches!(
        evidence.instantiate_child(
            first,
            &crate::parser::ExpandedForChild::Bound(&wrong_member),
            &child
        ),
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
    match resource.child_secrecy() {
        ChildSecrecyRead::NotLoopChild => {}
        unexpected @ (ChildSecrecyRead::Row(_)
        | ChildSecrecyRead::Unavailable(_)
        | ChildSecrecyRead::RowMismatch) => {
            panic!("expected a static resource, got {unexpected:?}")
        }
    }
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
        resource.id.identity_str().is_some_and(|id| {
            crate::resource::decompose_binding_address(id)
                .is_some_and(|address| address.is_direct_child_of("children"))
        })
    }) {
        match child.child_secrecy() {
            ChildSecrecyRead::Unavailable(reason) => assert_eq!(
                reason,
                TemplateEvidenceUnavailable::ConfigurationNotAdmitted
            ),
            unexpected @ (ChildSecrecyRead::NotLoopChild
            | ChildSecrecyRead::Row(_)
            | ChildSecrecyRead::RowMismatch) => {
                panic!("expected unavailable child evidence, got {unexpected:?}")
            }
        }
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
    match other.child_secrecy() {
        ChildSecrecyRead::RowMismatch => {}
        unexpected @ (ChildSecrecyRead::NotLoopChild
        | ChildSecrecyRead::Row(_)
        | ChildSecrecyRead::Unavailable(_)) => {
            panic!("expected transplanted row mismatch, got {unexpected:?}")
        }
    }
    assert_eq!(
        graph.insert_child(&other),
        Err(ChildEvidenceInsertError::ChildDeclaration)
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
    match child.child_secrecy() {
        ChildSecrecyRead::RowMismatch => {}
        unexpected @ (ChildSecrecyRead::NotLoopChild
        | ChildSecrecyRead::Row(_)
        | ChildSecrecyRead::Unavailable(_)) => {
            panic!("expected provider row mismatch, got {unexpected:?}")
        }
    }
    assert_eq!(
        graph.insert_child(&child),
        Err(ChildEvidenceInsertError::ChildDeclaration)
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
                child.id.set_identity("children.b".into());
                child.binding =
                    Some(crate::resource::RootScope::new().resolve("children.b".into()));
            }
            _ => unreachable!(),
        }
        let mut graph = graph(&file).clone();
        match child.child_secrecy() {
            ChildSecrecyRead::RowMismatch => {}
            unexpected @ (ChildSecrecyRead::NotLoopChild
            | ChildSecrecyRead::Row(_)
            | ChildSecrecyRead::Unavailable(_)) => {
                panic!("{mutation}: expected row mismatch, got {unexpected:?}")
            }
        }
        assert_eq!(
            graph.insert_child(&child),
            Err(ChildEvidenceInsertError::ChildDeclaration),
            "{mutation}"
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
    match child.child_secrecy() {
        ChildSecrecyRead::RowMismatch => {}
        unexpected @ (ChildSecrecyRead::NotLoopChild
        | ChildSecrecyRead::Row(_)
        | ChildSecrecyRead::Unavailable(_)) => {
            panic!("expected anonymous row mismatch, got {unexpected:?}")
        }
    }
    assert_eq!(
        graph.insert_child(child),
        Err(ChildEvidenceInsertError::ChildDeclaration)
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
        let row = match child.child_secrecy() {
            ChildSecrecyRead::Row(row) => row,
            unexpected @ (ChildSecrecyRead::NotLoopChild
            | ChildSecrecyRead::Unavailable(_)
            | ChildSecrecyRead::RowMismatch) => {
                panic!("normalization must retain the checked child's row, got {unexpected:?}")
            }
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
    let member = admitted_member(first, "a");
    assert!(matches!(
        evidence.instantiate_child(
            second,
            &crate::parser::ExpandedForChild::Bound(&member),
            &child
        ),
        Err(TemplateEvidenceMismatch::TemplateDeclaration)
    ));
    assert!(matches!(
        evidence.instantiate_child(
            first,
            &crate::parser::ExpandedForChild::Bound(&member),
            &child
        ),
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
        match resource.child_secrecy() {
            ChildSecrecyRead::NotLoopChild => {}
            unexpected @ (ChildSecrecyRead::Row(_)
            | ChildSecrecyRead::Unavailable(_)
            | ChildSecrecyRead::RowMismatch) => {
                panic!("expected a static resource, got {unexpected:?}")
            }
        }
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
        match child.child_secrecy() {
            ChildSecrecyRead::NotLoopChild => {}
            unexpected @ (ChildSecrecyRead::Row(_)
            | ChildSecrecyRead::Unavailable(_)
            | ChildSecrecyRead::RowMismatch) => panic!(
                "expected an eagerly expanded child without a deferred judgement, got {unexpected:?}"
            ),
        }
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
        .find(|resource| resource.id.identity_str() == Some("children.a"))
        .unwrap();
    match child.child_secrecy() {
        ChildSecrecyRead::Row(_) => {}
        unexpected @ (ChildSecrecyRead::NotLoopChild
        | ChildSecrecyRead::Unavailable(_)
        | ChildSecrecyRead::RowMismatch) => {
            panic!("expected deferred child's row, got {unexpected:?}")
        }
    }
    match child.instantiate().child_secrecy() {
        ChildSecrecyRead::NotLoopChild => {}
        unexpected @ (ChildSecrecyRead::Row(_)
        | ChildSecrecyRead::Unavailable(_)
        | ChildSecrecyRead::RowMismatch) => {
            panic!("reinstantiation must clear the child judgement, got {unexpected:?}")
        }
    }
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

fn assert_foreign_template_rejected(entry_point: &str) {
    let (_directory_a, file_a, schemas_a, context_a) = directory(ROOT_LOOP);
    let file_a = resolved(file_a, &schemas_a, &context_a);
    let (_directory_b, file_b, schemas_b, context_b) = directory(ROOT_LOOP);
    let mut file_b = resolved(file_b, &schemas_b, &context_b);
    assert!(matches!(
        file_a.deferred_for_expressions()[0].secrecy(),
        TemplateSecrecy::Derived(_)
    ));
    match entry_point {
        "append_finalized" => {
            file_b.take_deferred_for_expressions();
            file_b
                .deferred_for_expressions_mut()
                .append_finalized(file_a.into_deferred_for_expressions());
        }
        "replace_deferred_for_expressions" => {
            file_b.replace_deferred_for_expressions(file_a.into_deferred_for_expressions());
        }
        "deferred_for_expressions_mut" => {
            *file_b
                .deferred_for_expressions_mut()
                .iter_mut()
                .next()
                .unwrap() = file_a.deferred_for_expressions()[0].clone();
        }
        _ => unreachable!(),
    }
    let template = &file_b.deferred_for_expressions()[0];
    let error = graph(&file_b).template_evidence(template).unwrap_err();
    assert_eq!(
        error,
        TemplateAdmissionError::ForeignTemplateEvidence {
            resource: template.template_resource_id().clone(),
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "deferred-for template `{}` carries secrecy evidence this configuration did not admit",
            template.template_resource_id()
        )
    );
    let before = file_b.clone();
    // Unresolved loop-slot values deliberately compare unequal. Compare the
    // structural representation and check the retained capability separately.
    let template_before = format!("{:?}", file_b.deferred_for_expressions()[0]);
    let bindings = crate::binding_index::IterableBindings::from_upstream_only(
        crate::upstream_exports::remote_bindings_for_test(HashMap::from([(
            "upstream".into(),
            HashMap::from([("items".into(), list(&["a", "b"]))]),
        )])),
    );
    let error = crate::validation::deferred_types::with_session_for_test(&context_b, |session| {
        file_b.expand_deferred_for_expressions(&bindings, session)
    })
    .expect_err("a template from another admission must not supply this graph's child evidence");
    let child_id = children(
        &before.deferred_for_expressions()[0],
        list(&["a"]),
        &context_b,
    )
    .remove(0)
    .id;
    assert!(matches!(
        &error,
        crate::parser::DeferredForExpansionError::ChildSecrecy(
            ChildEvidenceInsertError::ForeignTemplate { child }
        ) if child == &child_id
    ));
    assert_eq!(
        error.to_string(),
        format!(
            "deferred child `{child_id}` carries secrecy evidence from a template this configuration did not admit"
        )
    );
    assert_eq!(file_b.resources, before.resources);
    assert_eq!(file_b.warnings, before.warnings);
    assert_eq!(
        file_b.deferred_for_expressions().len(),
        before.deferred_for_expressions().len()
    );
    assert_eq!(
        format!("{:?}", file_b.deferred_for_expressions()[0]),
        template_before
    );
    assert!(matches!(
        (
            file_b.deferred_for_expressions()[0].secrecy(),
            before.deferred_for_expressions()[0].secrecy()
        ),
        (TemplateSecrecy::Derived(after), TemplateSecrecy::Derived(before))
            if std::sync::Arc::ptr_eq(after, before)
    ));
    assert!(std::sync::Arc::ptr_eq(
        file_b.authored_inputs.as_ref().unwrap(),
        before.authored_inputs.as_ref().unwrap()
    ));
    assert!(std::sync::Arc::ptr_eq(
        &graph(&file_b).rows,
        &graph(&before).rows
    ));
    assert!(std::sync::Arc::ptr_eq(
        &graph(&file_b).templates,
        &graph(&before).templates
    ));
}

#[test]
fn issue_4516_foreign_template_via_append_finalized_is_rejected() {
    assert_foreign_template_rejected("append_finalized");
}

#[test]
fn issue_4516_foreign_template_via_replace_is_rejected() {
    assert_foreign_template_rejected("replace_deferred_for_expressions");
}

#[test]
fn issue_4516_foreign_template_via_mutation_is_rejected() {
    assert_foreign_template_rejected("deferred_for_expressions_mut");
}

#[test]
fn issue_4516_own_template_expands_with_cloned_graph() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let before = graph(&file).clone();
    let mut expanded = file.clone();
    expand(&mut expanded, list(&["a", "b"]), &context);
    assert!(expanded.deferred_for_expressions().is_empty());
    assert_child_row(&expanded, "children.a");
    assert_child_row(&expanded, "children.b");
    assert!(std::sync::Arc::ptr_eq(&before.rows, &graph(&file).rows));
    assert!(!std::sync::Arc::ptr_eq(
        &before.rows,
        &graph(&expanded).rows
    ));
    assert!(std::sync::Arc::ptr_eq(
        &before.templates,
        &graph(&expanded).templates
    ));
}

#[test]
fn issue_4516_foreign_template_evidence_cannot_replace_derived_evidence() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let file = resolved(file, &schemas, &context);
    let mut template = file.deferred_for_expressions()[0].clone();
    let TemplateSecrecy::Derived(evidence) = template.secrecy() else {
        panic!("admitted template must carry evidence");
    };
    // Equal slots and equal evidence are insufficient: admission owns the Arc.
    template.attach_template_evidence(std::sync::Arc::new(evidence.as_ref().clone()));
    let error = graph(&file).template_evidence(&template).unwrap_err();
    assert_eq!(
        error,
        TemplateAdmissionError::ForeignTemplateEvidence {
            resource: template.template_resource_id().clone(),
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "deferred-for template `{}` carries secrecy evidence this configuration did not admit",
            template.template_resource_id()
        )
    );
}
