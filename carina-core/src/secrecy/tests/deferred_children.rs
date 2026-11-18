use super::{context, schemas};
use crate::parser::{ModuleResolvedInferredFile, ProviderContext};
use crate::schema::{
    PatternAttributeSchema as Attribute, PatternAttributeType as Type,
    PatternResourceSchema as Schema, SchemaRegistry,
};
use crate::secrecy::SecrecyReport;

mod checked;
mod round2;

use crate::parser::{ExpandedDeferredForExpression, ExpandedInferredFile};
use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
use crate::secrecy::{Evidence, Secrecy};
use std::collections::HashMap;

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
        .unwrap()
        .parsed;
    let file = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            root, &base, session, &schemas,
        )
        .unwrap()
    });
    (directory, file, schemas, context)
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
    crate::validation::deferred_types::with_session_for_test(context, |session| {
        crate::validation::resolve_bare_binding_uses_in_session(
            file,
            schemas,
            session,
            &crate::module_resolver::ResolvedModuleSignatures::default(),
        )
        .unwrap()
        .into_file()
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

fn measure(shape: &str, main: &str) {
    let (_directory, file, schemas, context) = directory(main);
    let report = report(&file, &schemas, &context);
    println!(
        "T0 {shape}: deferred={}, pending={}, admitted={}, findings={}, rows={:?}",
        file.deferred_for_expressions().len(),
        report.is_pending(),
        report.admitted().is_some(),
        report.findings().len(),
        report
            .rows
            .iter()
            .map(|(id, row)| (id.to_string(), &row.attributes))
            .collect::<Vec<_>>()
    );
    assert!(report.is_pending());
}

#[test]
fn issue_4516_t0_deferred_for_mixed_resource_map_stays_pending() {
    measure(
        "map",
        r#"
let source = mock.test.Source { tags = { a = secret("canary") b = "public" } }
let children = for key, value in source.tags {
    mock.test.Resource { comment = value }
}
"#,
    );
}

#[test]
fn issue_4516_t0_deferred_for_mixed_resource_list_stays_pending() {
    measure(
        "list",
        r#"
let source = mock.test.Source { items = [secret("canary"), "public"] }
let children = for value in source.items {
    mock.test.Resource { comment = value }
}
"#,
    );
}

#[test]
fn issue_4516_t1_deferred_for_root_children_join_the_directory_graph() {
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
fn issue_4516_t2_deferred_for_module_children_join_the_directory_graph() {
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
fn issue_4516_t3_deferred_for_map_rows_follow_keys_after_removal() {
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
fn issue_4516_t3_deferred_for_list_rows_follow_indices_after_removal() {
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
fn issue_4516_t7_deferred_for_anonymous_rows_use_the_produced_id() {
    let (_directory, file, schemas, context) = directory(
        "for item in upstream.items { mock.test.Child { comment = secret('canary') identity_input = item } }\n",
    );
    let report = report(&file, &schemas, &context);
    assert!(
        !report.is_pending(),
        "template report is pending: findings={}, admitted={}",
        report.findings().len(),
        report.admitted().is_some()
    );
    let mut file = resolved(file, &schemas, &context);
    expand(&mut file, list(&["a", "b"]), &context);
    let before = file
        .resources
        .iter()
        .filter(|resource| resource.binding.is_none())
        .map(|resource| {
            assert_eq!(
                graph(&file).attributes(&resource.id).unwrap()["comment"],
                Evidence::Leaf(Secrecy::Secret)
            );
            resource.id.clone()
        })
        .collect::<Vec<_>>();
    assert_eq!(before.len(), 2);
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
        .map(|resource| graph(&file).attributes(&resource.id).is_some())
        .collect::<Vec<_>>();
    println!(
        "T7 deferred anonymous: produced_id_rows={}, final_id_rows={after:?}",
        before.len()
    );
}

#[test]
fn issue_4516_t7_static_anonymous_main_measurement() {
    let (_directory, file, schemas, context) =
        directory("mock.test.Child { comment = secret('canary') identity_input = 'static' }\n");
    let mut file = resolved(file, &schemas, &context);
    let resource = file
        .resources
        .iter()
        .find(|resource| resource.binding.is_none())
        .unwrap();
    let before = graph(&file).attributes(&resource.id).is_some();
    crate::identifier::compute_anonymous_identifiers_for_test(
        &mut file.resources,
        &file.providers,
        &schemas,
        &|_| Vec::new(),
    )
    .unwrap();
    let resource = file
        .resources
        .iter()
        .find(|resource| resource.binding.is_none())
        .unwrap();
    println!(
        "T7 static anonymous on main: produced_id_row={before}, final_id_row={}",
        graph(&file).attributes(&resource.id).is_some()
    );
}

#[test]
fn issue_4516_saved_plan_main_wire_measurement() {
    for bound in [true, false] {
        let binding = if bound { "let children = " } else { "" };
        let (_directory, file, schemas, context) = directory(&format!(
            "{binding}for item in upstream.items {{ mock.test.Child {{ comment = 'public' identity_input = item }} }}\n"
        ));
        let file = file.into_expanded(&schemas, &context).unwrap();
        let template: ExpandedDeferredForExpression = file.deferred_for_expressions()[0].clone();
        let template = template.into_compiled();
        println!(
            "T6 main wire (bound={bound}): {}",
            serde_json::to_string(&template).unwrap()
        );
    }
}
