use super::*;
use crate::binding_index::{BindingCompleteness, BindingValueSource};
use crate::parser::ProviderContext;
use crate::resource::{ExpandedConcreteValue as ConcreteValue, ExpandedValue as Value};
use crate::schema::{
    PatternAttributeSchema as Attribute, PatternAttributeType as Type,
    PatternResourceSchema as Schema,
};
use crate::secrecy::{
    ChildSecrecyRead, Evidence, Secrecy, TemplateEvidenceUnavailable, TemplateSecrecy,
};
use std::collections::{BTreeSet, HashMap};

#[test]
fn issue_4516_deferred_for_executor_carries_rows_and_saved_plan_unavailability() {
    for bound in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let binding = if bound { "let children = " } else { "" };
        // A sibling value binding avoids the literal-only quote hint that the
        // existing saved-plan decoder intentionally does not retain.
        std::fs::write(directory.path().join("main.crn"), format!("{binding}for item in upstream.items {{ mock.test.Child {{ comment = visiblex identity_input = item }} }}\nlet secret_children = for item in upstream.items {{ mock.test.Child {{ comment = secret('canary') identity_input = item }} }}\n")).unwrap();
        std::fs::write(
            directory.path().join("sibling.crn"),
            "let unrelated = mock.test.Child { identity_input = 'sibling' }\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("providers.crn"),
            "provider mock {}\nlet upstream = upstream_state { source = '../upstream' }\nlet visiblex = 'public'\n",
        )
        .unwrap();
        let mut schemas = SchemaRegistry::new();
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
        let context = ProviderContext::new(crate::EnvironmentSnapshot::empty());
        let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
        let root = crate::config_loader::load_configuration_with_config(&base, &context, &schemas)
            .unwrap()
            .parsed;
        crate::validation::deferred_types::with_session_for_test(&context, |session| {
            let file = crate::module_resolver::resolve_modules_with_providerless_config(
                root, &base, session, &schemas,
            )
            .unwrap();
            let file = crate::validation::resolve_bare_binding_uses_in_session(
                file,
                &schemas,
                session,
                &crate::module_resolver::ResolvedModuleSignatures::default(),
            )
            .unwrap()
            .into_file();
            let graph = crate::resolved_graph::resolve_expanded_graph(file.clone().into()).unwrap();
            let (template, dependencies) = graph
                .compile_deferred_for(file.deferred_for_expressions()[0].clone())
                .unwrap();
            let template: DeferredForExpression<crate::resource::SavedModuleSource> =
                template.into();
            assert!(matches!(template.secrecy(), TemplateSecrecy::Derived(_)));
            let json = serde_json::to_value(&template).unwrap();
            let main_json = if bound {
                include_str!("main_bound.json")
            } else {
                include_str!("main_unbound.json")
            };
            assert_eq!(
                json,
                serde_json::from_str::<serde_json::Value>(main_json).unwrap()
            );
            assert_eq!(
                json.as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<BTreeSet<_>>(),
                BTreeSet::from([
                    "file",
                    "line",
                    "header",
                    "resource_type",
                    "target",
                    "iterable",
                    "binding",
                    "template_resource"
                ])
            );
            let saved: DeferredForExpression<crate::resource::SavedModuleSource> =
                serde_json::from_value(json.clone()).unwrap();
            assert!(matches!(
                saved.secrecy(),
                TemplateSecrecy::Unavailable(TemplateEvidenceUnavailable::SavedPlan)
            ));
            assert_eq!(serde_json::to_value(&saved).unwrap(), json);
            let mut bindings = ResolvedBindings::default();
            bindings.set_with_completeness(
                "upstream",
                HashMap::from([(
                    "items".to_string(),
                    Value::Concrete(ConcreteValue::List(vec![
                        Value::Concrete(ConcreteValue::String("a".into())),
                        Value::Concrete(ConcreteValue::String("b".into())),
                    ])),
                )]),
                BindingValueSource::Upstream,
                BindingCompleteness::Complete,
            );
            let identity_inputs = DeferredIdentityInputs {
                provider_configs: &file.providers,
                type_checks: session,
                factories: &[],
                schemas: &schemas,
            };
            let derived_children =
                materialize_deferred_create(&template, &dependencies, &bindings, &identity_inputs)
                    .unwrap();
            let saved_children =
                materialize_deferred_create(&saved, &dependencies, &bindings, &identity_inputs)
                    .unwrap();
            assert_eq!(derived_children, saved_children);
            assert_eq!(derived_children.len(), 2);
            for child in &derived_children {
                let Effect::Create(child) = child else {
                    panic!("materialization produces creates");
                };
                let row = match child.child_secrecy() {
                    ChildSecrecyRead::Row(row) => row,
                    unexpected @ (ChildSecrecyRead::NotLoopChild
                    | ChildSecrecyRead::Unavailable(_)
                    | ChildSecrecyRead::RowMismatch) => {
                        panic!("admitted child must carry a row, got {unexpected:?}")
                    }
                };
                assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Public));
                assert_eq!(
                    row.attributes()["identity_input"],
                    Evidence::Leaf(Secrecy::Public)
                );
                assert!(child.id.identity().is_some());
                let decoded: crate::resource::SavedResource =
                    serde_json::from_value(serde_json::to_value(child.as_resource()).unwrap())
                        .unwrap();
                match decoded.child_secrecy() {
                    ChildSecrecyRead::NotLoopChild => {}
                    unexpected @ (ChildSecrecyRead::Row(_)
                    | ChildSecrecyRead::Unavailable(_)
                    | ChildSecrecyRead::RowMismatch) => {
                        panic!("direct resource decode has no child hand-over, got {unexpected:?}")
                    }
                }
            }
            for child in &saved_children {
                let Effect::Create(child) = child else {
                    panic!("materialization produces creates");
                };
                match child.child_secrecy() {
                    ChildSecrecyRead::Unavailable(reason) => {
                        assert_eq!(reason, TemplateEvidenceUnavailable::SavedPlan)
                    }
                    unexpected @ (ChildSecrecyRead::NotLoopChild
                    | ChildSecrecyRead::Row(_)
                    | ChildSecrecyRead::RowMismatch) => {
                        panic!("expected SavedPlan unavailability, got {unexpected:?}")
                    }
                }
            }
            let (secret_template, secret_dependencies) = graph
                .compile_deferred_for(file.deferred_for_expressions()[1].clone())
                .unwrap();
            let secret_children = materialize_deferred_create(
                &secret_template,
                &secret_dependencies,
                &bindings,
                &identity_inputs,
            )
            .unwrap();
            assert_eq!(secret_children.len(), 2);
            for child in &secret_children {
                let Effect::Create(child) = child else {
                    panic!("materialization produces creates");
                };
                let row = match child.child_secrecy() {
                    ChildSecrecyRead::Row(row) => row,
                    unexpected @ (ChildSecrecyRead::NotLoopChild
                    | ChildSecrecyRead::Unavailable(_)
                    | ChildSecrecyRead::RowMismatch) => {
                        panic!("admitted child must carry a row, got {unexpected:?}")
                    }
                };
                assert_eq!(row.attributes()["comment"], Evidence::Leaf(Secrecy::Secret));
            }
        });
    }
}
