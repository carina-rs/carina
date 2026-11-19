use super::{context, schemas};
use crate::parser::{ModuleResolvedInferredFile, ProviderContext};
use crate::schema::{
    PatternAttributeSchema as Attribute, PatternAttributeType as Type,
    PatternResourceSchema as Schema, SchemaRegistry,
};
use crate::secrecy::SecrecyReport;

mod checked;
mod readers;

use crate::parser::{ExpandedDeferredForExpression, ExpandedInferredFile};
use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
use crate::secrecy::{ChildSecrecy, Evidence, Secrecy};
use std::collections::{BTreeMap, HashMap};

const ROOT_LOOP: &str = r#"
let children = for item in upstream.items {
    mock.test.Resource { comment = secret("canary") name = item }
}
"#;

fn directory(
    main: &str,
) -> (
    tempfile::TempDir,
    ModuleResolvedInferredFile,
    SchemaRegistry,
    ProviderContext,
) {
    fixture(main, None)
}

fn fixture(
    main: &str,
    module: Option<&str>,
) -> (
    tempfile::TempDir,
    ModuleResolvedInferredFile,
    SchemaRegistry,
    ProviderContext,
) {
    try_fixture(main, module).unwrap()
}

fn try_fixture(
    main: &str,
    module: Option<&str>,
) -> Result<
    (
        tempfile::TempDir,
        ModuleResolvedInferredFile,
        SchemaRegistry,
        ProviderContext,
    ),
    String,
> {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("main.crn"), main).unwrap();
    if let Some(module) = module {
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(directory.path().join("child/main.crn"), module).unwrap();
        std::fs::write(
            directory.path().join("child/sibling.crn"),
            "let module_unrelated = mock.test.Resource { comment = 'module sibling' }\n",
        )
        .unwrap();
    }
    std::fs::write(
        directory.path().join("sibling.crn"),
        "let unrelated = mock.test.Resource { comment = 'sibling' }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("providers.crn"),
        "provider mock {}\nlet upstream = upstream_state { source = '../upstream' }\n",
    )
    .unwrap();
    let mut schemas = schemas();
    schemas.insert(
        "mock",
        Schema::new("test.Source")
            .attribute(Attribute::optional("tags", Type::map(Type::string())).build())
            .attribute(Attribute::optional("items", Type::list(Type::string())).build())
            .validate()
            .unwrap(),
    );
    schemas.insert(
        "mock",
        Schema::new("test.Child")
            .attribute(Attribute::optional("comment", Type::string()).build())
            .attribute(
                Attribute::optional("identity_input", Type::string())
                    .create_only()
                    .build(),
            )
            .validate()
            .unwrap(),
    );
    let context = context();
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let root = crate::config_loader::load_configuration_with_config(&base, &context, &schemas)
        .map_err(|error| format!("configuration: {error:?}"))?
        .parsed;
    let file = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            root, &base, session, &schemas,
        )
        .map_err(|error| format!("module resolution: {error:?}"))
    })?;
    Ok((directory, file, schemas, context))
}

fn report(
    file: &ModuleResolvedInferredFile,
    schemas: &SchemaRegistry,
    context: &ProviderContext,
) -> SecrecyReport {
    crate::config_loader::load_buffer_configuration(file, schemas, context)
        .judged()
        .secrecy()
        .clone()
}

fn resolved(
    file: ModuleResolvedInferredFile,
    schemas: &SchemaRegistry,
    context: &ProviderContext,
) -> ExpandedInferredFile {
    resolve_owner(file, schemas, context).into_file()
}

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

fn list(items: &[&str]) -> Value {
    Value::Concrete(ConcreteValue::List(
        items
            .iter()
            .map(|item| Value::Concrete(ConcreteValue::String((*item).into())))
            .collect(),
    ))
}

fn map(items: &[&str]) -> Value {
    Value::Concrete(ConcreteValue::Map(
        items
            .iter()
            .map(|item| {
                (
                    (*item).into(),
                    Value::Concrete(ConcreteValue::String((*item).into())),
                )
            })
            .collect(),
    ))
}

fn expand(file: &mut ExpandedInferredFile, iterable: Value, context: &ProviderContext) {
    let bindings = crate::binding_index::IterableBindings::from_upstream_only(
        crate::upstream_exports::remote_bindings_for_test(HashMap::from([(
            "upstream".to_string(),
            HashMap::from([("items".to_string(), iterable)]),
        )])),
    );
    crate::validation::deferred_types::with_session_for_test(context, |session| {
        file.expand_deferred_for_expressions(&bindings, session)
    })
    .unwrap();
}

fn graph(file: &ExpandedInferredFile) -> &crate::secrecy::ValidatedSecrecyGraph {
    file.authored_inputs().unwrap().validated_secrecy().unwrap()
}

fn assert_child_row(file: &ExpandedInferredFile, binding: &str) {
    let child = file
        .resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|address| address.as_str() == binding)
        })
        .unwrap();
    let attributes = graph(file)
        .attributes(&child.id)
        .unwrap_or_else(|| panic!("missing child row for {}", child.id));
    assert_eq!(attributes["comment"], Evidence::Leaf(Secrecy::Secret));
    assert_eq!(attributes["name"], Evidence::Leaf(Secrecy::Public));
}

fn assert_mixed_resource_iterable_pending(main: &str, attribute: &str, expected: Evidence) {
    let (_directory, file, schemas, context) = directory(main);
    let report = report(&file, &schemas, &context);
    assert_eq!(file.deferred_for_expressions().len(), 1);
    assert!(report.is_pending());
    assert!(report.admitted().is_none());
    assert!(report.findings().is_empty());
    assert_eq!(super::attribute(&report, "source", attribute), expected);
}

#[test]
fn issue_4516_deferred_for_mixed_resource_map_stays_pending() {
    assert_mixed_resource_iterable_pending(
        r#"
let source = mock.test.Source { tags = { a = secret("canary") b = "public" } }
let children = for key, value in source.tags {
    mock.test.Resource { comment = value }
}
"#,
        "tags",
        Evidence::Map(
            BTreeMap::from([
                ("a".into(), Evidence::Leaf(Secrecy::Secret)),
                ("b".into(), Evidence::Leaf(Secrecy::Public)),
            ])
            .into(),
        ),
    );
}

#[test]
fn issue_4516_deferred_for_mixed_resource_list_stays_pending() {
    assert_mixed_resource_iterable_pending(
        r#"
let source = mock.test.Source { items = [secret("canary"), "public"] }
let children = for value in source.items {
    mock.test.Resource { comment = value }
}
"#,
        "items",
        Evidence::List(
            vec![
                Evidence::Leaf(Secrecy::Secret),
                Evidence::Leaf(Secrecy::Public),
            ]
            .into(),
        ),
    );
}

#[test]
fn issue_4516_deferred_for_root_children_join_the_directory_graph() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let report = report(&file, &schemas, &context);
    assert!(
        !report.is_pending(),
        "template report is pending: findings={}, admitted={}",
        report.findings().len(),
        report.admitted().is_some()
    );
    assert!(report.admitted().is_some());
    let mut file = resolved(file, &schemas, &context);
    expand(&mut file, list(&["a", "b"]), &context);
    assert_child_row(&file, "children[0]");
    assert_child_row(&file, "children[1]");
    let unrelated = file
        .resources
        .iter()
        .find(|resource| {
            resource
                .binding
                .as_ref()
                .is_some_and(|binding| binding.as_str() == "unrelated")
        })
        .unwrap();
    assert_eq!(
        graph(&file).attributes(&unrelated.id).unwrap()["comment"],
        Evidence::Leaf(Secrecy::Public)
    );
}

#[test]
fn issue_4516_deferred_for_module_children_join_the_directory_graph() {
    let (_directory, file, schemas, context) = fixture(
        "let child = use { source = './child' }\nlet instance = child { items = upstream.items }\n",
        Some(
            "arguments { items: list(String) }\nlet children = for item in items { mock.test.Resource { comment = secret('canary') name = item } }\n",
        ),
    );
    let report = report(&file, &schemas, &context);
    assert!(
        !report.is_pending(),
        "template report is pending: findings={}, admitted={}",
        report.findings().len(),
        report.admitted().is_some()
    );
    assert!(report.admitted().is_some());
    let mut file = resolved(file, &schemas, &context);
    expand(&mut file, list(&["a", "b"]), &context);
    assert_child_row(&file, "instance.children[0]");
    assert_child_row(&file, "instance.children[1]");
    for binding in ["unrelated", "instance.module_unrelated"] {
        let resource = file
            .resources
            .iter()
            .find(|resource| {
                resource
                    .binding
                    .as_ref()
                    .is_some_and(|address| address.as_str() == binding)
            })
            .unwrap();
        assert!(graph(&file).attributes(&resource.id).is_some());
    }
}

#[test]
fn issue_4516_deferred_for_map_rows_follow_keys_after_removal() {
    let (_directory, file, schemas, context) = directory(
        "let children = for key, item in upstream.items { mock.test.Resource { comment = secret('canary') name = item } }\n",
    );
    let report = report(&file, &schemas, &context);
    assert!(
        !report.is_pending(),
        "template report is pending: findings={}, admitted={}",
        report.findings().len(),
        report.admitted().is_some()
    );
    let file = resolved(file, &schemas, &context);
    let mut both = file.clone();
    expand(&mut both, map(&["a", "b"]), &context);
    assert_child_row(&both, "children.b");
    let mut only_b = file;
    expand(&mut only_b, map(&["b"]), &context);
    assert_child_row(&only_b, "children.b");
}

#[test]
fn issue_4516_deferred_for_list_rows_follow_indices_after_removal() {
    let (_directory, file, schemas, context) = directory(ROOT_LOOP);
    let report = report(&file, &schemas, &context);
    assert!(
        !report.is_pending(),
        "template report is pending: findings={}, admitted={}",
        report.findings().len(),
        report.admitted().is_some()
    );
    let file = resolved(file, &schemas, &context);
    let mut both = file.clone();
    expand(&mut both, list(&["a", "b"]), &context);
    assert_child_row(&both, "children[1]");
    let mut only_b = file;
    expand(&mut only_b, list(&["b"]), &context);
    assert_child_row(&only_b, "children[0]");
}

#[test]
fn issue_4516_anonymous_deferred_children_keep_child_secrecy_after_identifier_computation() {
    let (_directory, file, schemas, context) = directory(
        "for item in upstream.items { mock.test.Child { comment = secret('canary') identity_input = item } }\n",
    );
    let mut file = resolved(file, &schemas, &context);
    expand(&mut file, list(&["a", "b"]), &context);
    crate::identifier::compute_anonymous_identifiers_for_test(
        &mut file.resources,
        &file.providers,
        &schemas,
        &|_| Vec::new(),
    )
    .unwrap();
    let children = file
        .resources
        .iter()
        .filter(|resource| resource.binding.is_none())
        .collect::<Vec<_>>();
    assert_eq!(children.len(), 2);
    for child in children {
        assert!(child.id.identity().is_some());
        let Some(ChildSecrecy::Row(row)) = child.child_secrecy() else {
            panic!("anonymous identity computation must retain the child's row");
        };
        assert_eq!(row.attributes().len(), 2);
        assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Secret));
        assert_eq!(
            row.attributes()["identity_input"],
            Evidence::Leaf(Secrecy::Public)
        );
    }
}

#[test]
fn issue_4516_anonymous_graph_keying_matches_main_and_belongs_to_carina_4442() {
    // Current keying on main retains the pre-identity key for static anonymous
    // resources too. Rekeying after identity computation belongs to carina#4442.
    for (main, count) in [
        (
            "for item in upstream.items { mock.test.Child { comment = secret('canary') identity_input = item } }\n",
            2,
        ),
        (
            "mock.test.Child { comment = secret('canary') identity_input = 'static' }\n",
            1,
        ),
    ] {
        let (_directory, file, schemas, context) = directory(main);
        let mut file = resolved(file, &schemas, &context);
        expand(&mut file, list(&["a", "b"]), &context);
        let before = file
            .resources
            .iter()
            .filter(|resource| resource.binding.is_none())
            .map(|resource| {
                (
                    resource.id.clone(),
                    graph(&file).attributes(&resource.id).unwrap().clone(),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(before.len(), count);
        crate::identifier::compute_anonymous_identifiers_for_test(
            &mut file.resources,
            &file.providers,
            &schemas,
            &|_| Vec::new(),
        )
        .unwrap();
        let after = file
            .resources
            .iter()
            .filter(|resource| resource.binding.is_none())
            .collect::<Vec<_>>();
        assert_eq!(after.len(), count);
        for (resource, (pre_identity_id, attributes)) in after.into_iter().zip(before) {
            assert_ne!(resource.id, pre_identity_id);
            assert!(graph(&file).attributes(&resource.id).is_none());
            assert_eq!(graph(&file).attributes(&pre_identity_id), Some(&attributes));
        }
    }
}

#[test]
fn issue_4516_resolved_api_owns_the_cli_expansion_graph() {
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
    assert!(
        same_graph,
        "ResolvedFile and its inner file must read one stored graph"
    );
    assert!(
        visible_after_rewrap,
        "ResolvedFile must read the expanded graph from its inner file"
    );
}

fn assert_anonymous_templates(
    file: ModuleResolvedInferredFile,
    schemas: &SchemaRegistry,
    context: &ProviderContext,
) {
    let report = report(&file, schemas, context);
    let pending = report.is_pending();
    let admitted = report.admitted().is_some();
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
    assert_eq!(file.deferred_for_expressions().len(), 2);
    assert_eq!(ids.len(), 2);
    assert_eq!(keys.len(), 2);
    assert!(report.findings().is_empty());
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
fn issue_4516_two_unbound_templates_of_one_type_are_admitted() {
    let (_directory, file, schemas, context) = directory(
        "for item in upstream.items { mock.test.Child { comment = secret('first') identity_input = item } }\nfor item in upstream.items { mock.test.Child { comment = secret('second') identity_input = item } }\n",
    );
    assert_anonymous_templates(file, &schemas, &context);
}

#[test]
fn issue_4516_unbound_module_template_called_twice_is_admitted() {
    let (_directory, file, schemas, context) = fixture(
        "let child = use { source = './child' }\nlet first = child { items = upstream.items }\nlet second = child { items = upstream.items }\n",
        Some(
            "arguments { items: list(String) }\nfor item in items { mock.test.Child { comment = secret('canary') identity_input = item } }\n",
        ),
    );
    assert_anonymous_templates(file, &schemas, &context);
}
