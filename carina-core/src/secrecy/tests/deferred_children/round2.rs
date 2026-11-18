use super::*;
use crate::secrecy::ChildSecrecy;

fn resolve_owner(
    file: ModuleResolvedInferredFile,
    schemas: &SchemaRegistry,
    context: &ProviderContext,
) -> crate::validation::ResolvedFile<crate::parser::ExpandedInferredExportParam> {
    crate::validation::deferred_types::with_session_for_test(context, |session| {
        crate::validation::resolve_bare_binding_uses_in_session(
            file,
            schemas,
            session,
            &crate::module_resolver::ResolvedModuleSignatures::default(),
        )
        .unwrap()
    })
}

#[test]
fn issue_4516_round2_f1_static_resource_has_no_child_judgement() {
    let (_directory, file, schemas, context) =
        directory("let ordinary = mock.test.Resource { comment = 'public' }\n");
    let file = resolved(file, &schemas, &context);
    assert!(file.deferred_for_expressions().is_empty());
    for resource in &file.resources {
        assert!(resource.child_secrecy().is_none());
    }
}

#[test]
fn issue_4516_round2_f1_static_for_child_has_no_child_judgement() {
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
fn issue_4516_round2_f1_deferred_child_has_judgement_and_reinstance_clears_it() {
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
fn issue_4516_round2_f2_resolved_api_owns_the_cli_expansion_graph() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let owner = resolve_owner(file, &schemas, &context);
    let same_graph = std::ptr::eq(owner.validated_secrecy().unwrap(), graph(owner.as_file()));
    // The CLI consumes ResolvedFile before mutating its ExpandedFile. There is
    // no mutable ResolvedFile API through which to read the old copy afterward.
    let mut file = owner.into_file();
    let (selected, retained) = file.take_deferred_for_expressions().partition(|_| true);
    file.replace_deferred_for_expressions(selected);
    expand(&mut file, list(&["a", "b"]), &context);
    file.deferred_for_expressions_mut()
        .append_finalized(retained);
    assert_child_row(&file, "children[0]");
    assert_child_row(&file, "children[1]");
    let owner = crate::validation::ResolvedFile::for_test(file);
    let child = owner
        .resources
        .iter()
        .find(|resource| resource.id.identity_str() == Some("children[0]"))
        .unwrap();
    let visible_after_rewrap = owner
        .validated_secrecy()
        .is_ok_and(|graph| graph.attributes(&child.id).is_some());
    println!(
        "F2 real API: same_graph={same_graph}, CLI_expanded_child_rows=2, ResolvedFile_rewrap_child_visible={visible_after_rewrap}"
    );
    assert!(
        same_graph,
        "ResolvedFile and its inner file must read one stored graph"
    );
    assert!(
        visible_after_rewrap,
        "ResolvedFile must read the expanded graph from its inner file"
    );
}

fn measure_anonymous_templates(
    label: &str,
    file: ModuleResolvedInferredFile,
    schemas: &SchemaRegistry,
    context: &ProviderContext,
) {
    let report = report(&file, schemas, context);
    let pending = report.is_pending();
    let admitted = report.admitted().is_some();
    println!(
        "F3(a) {label}: deferred={}, pending={pending}, admitted={admitted}, findings={}",
        file.deferred_for_expressions().len(),
        report.findings().len()
    );
    let ids = file
        .deferred_for_expressions()
        .iter()
        .map(|template| template.template_resource_id().clone())
        .collect::<std::collections::HashSet<_>>();
    let keys = file
        .deferred_for_expressions()
        .iter()
        .map(|template| {
            let slots = &template.template_resource().as_resource().authored_slots;
            (slots.declaration_id().cloned(), slots.instance())
        })
        .collect::<std::collections::HashSet<_>>();
    println!(
        "F3(a) {label}: distinct_ResourceIds={}, distinct_declaration_instances={}",
        ids.len(),
        keys.len()
    );
    assert_eq!(file.deferred_for_expressions().len(), 2);
    let mut file = resolve_owner(file, schemas, context).into_file();
    expand(&mut file, list(&["a", "b"]), context);
    let children = file
        .resources
        .iter()
        .filter(|resource| resource.binding.is_none())
        .collect::<Vec<_>>();
    let rows = children
        .iter()
        .filter(|child| matches!(child.child_secrecy(), Some(ChildSecrecy::Row(_))))
        .count();
    println!(
        "F3(a) {label}: expansion=Ok, children={}, Row={rows}",
        children.len()
    );
    assert!(
        !pending,
        "anonymous template identities must not make the directory pending"
    );
    assert!(admitted);
    assert_eq!(children.len(), 4);
    assert_eq!(rows, children.len());
    for child in children {
        let Some(ChildSecrecy::Row(row)) = child.child_secrecy() else {
            unreachable!()
        };
        assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Secret));
        assert_eq!(graph(&file).attributes(&child.id), Some(row.attributes()));
    }
}

#[test]
fn issue_4516_round2_f3_two_unbound_templates_of_one_type_are_admitted() {
    let (_directory, file, schemas, context) = directory(
        "for item in upstream.items { mock.test.Child { comment = secret('first') identity_input = item } }\nfor item in upstream.items { mock.test.Child { comment = secret('second') identity_input = item } }\n",
    );
    measure_anonymous_templates("two unbound templates", file, &schemas, &context);
}

#[test]
fn issue_4516_round2_f3_unbound_module_template_called_twice_is_admitted() {
    let (_directory, file, schemas, context) = fixture(
        "let child = use { source = './child' }\nlet first = child { items = upstream.items }\nlet second = child { items = upstream.items }\n",
        Some(
            "arguments { items: list(String) }\nfor item in items { mock.test.Child { comment = secret('canary') identity_input = item } }\n",
        ),
    );
    measure_anonymous_templates("unbound module called twice", file, &schemas, &context);
}

#[test]
fn issue_4516_round2_f4_public_template_mutation_rejection_measurement() {
    use crate::secrecy::{ChildEvidenceInsertError, TemplateEvidenceUnavailable, TemplateSecrecy};
    for append in [false, true] {
        // Admission happens before adding any template, so this public API
        // sequence is also possible on main, where templates are pending.
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
        println!(
            "F4 public mutation (append={append}): admitted=true, template=ConfigurationNotAdmitted, expansion={result:?}"
        );
        // Record the draft's new rejection without choosing its compatibility policy.
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
