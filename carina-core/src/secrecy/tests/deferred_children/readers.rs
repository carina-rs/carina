use super::*;

use crate::module_resolver::ModuleError;
use crate::resource::knowledge::{Classification, KnowledgeContext};
use crate::validation::ParameterValueFinding;

#[derive(Debug)]
enum Rejection {
    ArgumentValueAccess,
    ExportsInModule,
}

#[derive(Debug)]
enum Expectation {
    Pending,
    Rejected(Rejection),
    Admitted,
}

use Expectation::{Admitted, Pending, Rejected};
use Rejection::{ArgumentValueAccess, ExportsInModule};

fn reader_fixture(
    main: &str,
    module: Option<&str>,
) -> (
    tempfile::TempDir,
    Result<ModuleResolvedInferredFile, ModuleError>,
    SchemaRegistry,
    ProviderContext,
) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("main.crn"), main).unwrap();
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
    if let Some(module) = module {
        std::fs::create_dir(directory.path().join("child")).unwrap();
        std::fs::write(directory.path().join("child/main.crn"), module).unwrap();
        std::fs::write(
            directory.path().join("child/sibling.crn"),
            "let module_unrelated = mock.test.Resource { comment = 'module sibling' }\n",
        )
        .unwrap();
    }
    let schemas = schemas();
    let context = context();
    let base = crate::paths::ResolvedPath::new(directory.path()).unwrap();
    let loaded =
        crate::config_loader::load_configuration_report(&base, &context, &schemas).unwrap();
    assert!(loaded.duplicate_declarations.is_empty());
    let file = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::module_resolver::resolve_modules_with_providerless_config(
            loaded.parsed,
            &base,
            session,
            &schemas,
        )
    });
    (directory, file, schemas, context)
}

fn assert_reader_protected(
    label: &str,
    selector: &str,
    map: bool,
    module: bool,
    export: bool,
    annotated: bool,
    expected: Expectation,
) {
    assert_reader_protected_for_iterable(
        label,
        selector,
        if map {
            ReaderIterable::Map
        } else {
            ReaderIterable::IndexedList
        },
        module,
        export,
        annotated,
        expected,
    );
}

enum ReaderIterable {
    IndexedList,
    ValueKeyedList,
    Map,
}

fn assert_reader_protected_for_iterable(
    label: &str,
    selector: &str,
    iterable: ReaderIterable,
    module: bool,
    export: bool,
    annotated: bool,
    expected: Expectation,
) {
    let (header, iterable_type) = match iterable {
        ReaderIterable::IndexedList => ("(i, item)", "list"),
        ReaderIterable::ValueKeyedList => ("item", "list"),
        ReaderIterable::Map => ("key, item", "map"),
    };
    // The colliding key selects `name`, so make that attribute secret too.
    let name = if selector == "children.comment.name" {
        "secret('name canary')"
    } else {
        "item"
    };
    let body = format!(
        "let children = for {header} in {} {{ mock.test.Resource {{ comment = secret('canary') name = {name} }} }}\n{}\n",
        if module { "items" } else { "upstream.items" },
        if export {
            let annotation = if annotated { ": String" } else { "" };
            format!("exports {{ output{annotation} = {selector} }}")
        } else {
            format!("let other = mock.test.Resource {{ comment = {selector} }}")
        },
    );
    let module_source = format!("arguments {{ items: {iterable_type}(String) }}\n{body}");
    let main = if module {
        format!(
            "let child = use {{ source = './child' }}\nlet instance = child {{ items = upstream.items }}\n{}",
            if export {
                "let other = mock.test.Resource { comment = instance.output }\n"
            } else {
                ""
            },
        )
    } else {
        body
    };
    let (_directory, file, schemas, context) =
        reader_fixture(&main, module.then_some(module_source.as_str()));
    match expected {
        Rejected(expected) => {
            let error = file
                .err()
                .unwrap_or_else(|| panic!("{label}: expected Rejected({expected:?})"));
            assert!(
                matches!(
                    (&expected, error.root_cause()),
                    (ArgumentValueAccess, ModuleError::ArgumentValueAccess { .. })
                        | (ExportsInModule, ModuleError::ExportsInModule)
                ),
                "{label}: expected Rejected({expected:?}), got {error:?}"
            );
        }
        Pending | Admitted => {
            let file = file.unwrap_or_else(|error| {
                panic!("{label}: expected {expected:?}, got Rejected({error:?})")
            });
            assert_eq!(file.deferred_for_expressions().len(), 1, "{label}");
            let report = report(&file, &schemas, &context);
            assert!(report.findings().is_empty(), "{label}: {report:?}");
            match expected {
                Pending => {
                    assert!(report.is_pending(), "{label}: expected Pending");
                    assert!(report.admitted().is_none(), "{label}: expected Pending");
                }
                Admitted => {
                    assert!(!report.is_pending(), "{label}: expected Admitted");
                    let graph = report
                        .admitted()
                        .unwrap_or_else(|| panic!("{label}: expected Admitted"));
                    assert_eq!(file.resources.len(), 1, "{label}");
                    let unrelated = &file.resources[0];
                    assert_eq!(unrelated.id.identity_str(), Some("unrelated"), "{label}");
                    assert_eq!(
                        graph.rows.keys().collect::<Vec<_>>(),
                        [&unrelated.id],
                        "{label}: only unrelated has a resource row before expansion"
                    );
                    assert!(
                        graph
                            .attributes(file.deferred_for_expressions()[0].template_resource_id())
                            .is_none(),
                        "{label}: the loop template is not a resource row"
                    );
                    let attributes = graph.attributes(&unrelated.id).unwrap();
                    assert_eq!(
                        attributes.keys().map(String::as_str).collect::<Vec<_>>(),
                        ["comment"],
                        "{label}"
                    );
                    assert_eq!(
                        attributes["comment"],
                        Evidence::Leaf(Secrecy::Public),
                        "{label}"
                    );
                    // The report carries no judgement for exports; secret exports are rejected by value (SecretExport).
                    crate::validation::deferred_types::with_session_for_test(&context, |session| {
                        crate::validation::check_export_values_for_validation(
                            &file.export_params,
                            &crate::binding_index::ResolvedBindings::for_expanded_validation(&file),
                            session,
                        )
                        .expect_ordinary_for_test()
                    })
                    .unwrap_or_else(|errors| {
                        panic!("{label}: expected the unexpanded list export to pass value validation, got {errors:?}")
                    });
                }
                Rejected(_) => unreachable!(),
            }
            let resolved =
                crate::validation::deferred_types::with_session_for_test(&context, |session| {
                    crate::validation::resolve_bare_binding_uses_in_session(
                        file,
                        &schemas,
                        session,
                        &crate::module_resolver::ResolvedModuleSignatures::default(),
                    )
                })
                .unwrap_or_else(|error| {
                    panic!("{label}: expected {expected:?}, got Rejected({error:?})")
                });
            assert_eq!(
                resolved.validated_secrecy().is_ok(),
                matches!(expected, Admitted),
                "{label}: expected {expected:?} after binding resolution"
            );
            if matches!(expected, Admitted) && export {
                let mut file = resolved.into_file();
                expand(&mut file, list(&["a", "b"]), &context);
                assert!(file.deferred_for_expressions().is_empty(), "{label}");
                assert_eq!(
                    file.resources.len(),
                    3,
                    "{label}: two children and unrelated"
                );
                let bindings = crate::binding_index::ResolvedBindings::pre_apply_live(
                    crate::binding_index::LivePreApplyInputs {
                        managed: &file.resources,
                        compositions: &file.compositions,
                        managed_current: &HashMap::new(),
                        data_source_values: &crate::data_source_value::DataSourceValues::empty(),
                        remote_bindings: &crate::upstream_exports::remote_bindings_for_test(
                            HashMap::new(),
                        ),
                        wait_aliases: &[],
                    },
                );
                let errors =
                    crate::validation::deferred_types::with_session_for_test(&context, |session| {
                        crate::validation::check_export_values_for_validation(
                            &file.export_params,
                            &bindings,
                            session,
                        )
                        .expect_ordinary_for_test()
                    })
                    .expect_err("list child export must be rejected as SecretExport");
                assert_eq!(errors.len(), 1, "{label}: {errors:?}");
                assert!(
                    matches!(errors[0].detail(), ParameterValueFinding::SecretExport { name } if name == "output"),
                    "{label}: expected SecretExport(output), got {errors:?}"
                );
                assert_eq!(
                    errors[0].detail().to_string(),
                    "export 'output' cannot hold a secret value",
                    "{label}"
                );
                let error =
                    crate::validation::deferred_types::with_session_for_test(&context, |session| {
                        crate::validation::check_export_values_pre_apply(
                            &file.export_params,
                            &bindings,
                            session,
                            crate::parser::retained::ProjectedPhase::plan_context(
                                &crate::resource::knowledge::KnowledgeContext::plan(&bindings),
                            ),
                        )
                    })
                    .expect_err("list child export must be rejected by the plan export check");
                assert_eq!(
                    error, "export 'output' cannot hold a secret value",
                    "{label}"
                );
            }
        }
    }
}

macro_rules! reader_test {
    ($name:ident, $selector:literal, $map:literal, $module:literal, $export:literal, $expected:expr) => {
        #[test]
        fn $name() {
            assert_reader_protected(
                stringify!($name),
                $selector,
                $map,
                $module,
                $export,
                true,
                $expected,
            );
        }
    };
}

reader_test!(
    issue_4516_reader_list,
    "children[0].comment",
    false,
    false,
    false,
    Pending
);

#[test]
fn for_value_keys_reader_list() {
    assert_reader_protected_for_iterable(
        "value-keyed list reader",
        "children.a.comment",
        ReaderIterable::ValueKeyedList,
        false,
        false,
        true,
        Pending,
    );
}

#[test]
#[ignore = "reader of a parse-time-expanded loop child stays pending: carina#4579"]
fn for_value_keys_reader_list_attribute_key() {
    assert_reader_protected_for_iterable(
        "value-keyed list attribute-name collision",
        "children.comment.name",
        ReaderIterable::ValueKeyedList,
        false,
        false,
        true,
        Pending,
    );
    let (_directory, file, schemas, context) = reader_fixture(
        "let children = for item in ['comment'] { mock.test.Resource { comment = secret('canary') name = secret('name canary') } }\nlet other = mock.test.Resource { comment = children.comment.name }\n",
        None,
    );
    let file = file.expect("literal value-keyed reader should load");
    assert!(file.deferred_for_expressions().is_empty());
    let report = report(&file, &schemas, &context);
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(!report.is_pending(), "{report:?}");
    assert!(report.admitted().is_some());
    assert_eq!(
        super::super::attribute(&report, "other", "comment"),
        Evidence::Leaf(Secrecy::Secret)
    );
    assert!(!format!("{report:?}").contains("canary"));
}

fn assert_literal_reader_admitted(header: &str, iterable: &str, selector: &str) {
    let source = format!(
        "let children = for {header} in {iterable} {{ mock.test.Resource {{ comment = secret('canary') name = secret('name canary') }} }}\nlet other = mock.test.Resource {{ comment = {selector} }}\n",
    );
    let (_directory, file, schemas, context) = reader_fixture(&source, None);
    let file = file.expect("literal reader should load");
    assert!(file.deferred_for_expressions().is_empty());
    let report = report(&file, &schemas, &context);
    let evidence = report
        .rows
        .iter()
        .find(|(id, _)| id.identity_str() == Some("other"))
        .and_then(|(_, row)| row.attributes.get("comment"));
    assert!(report.findings().is_empty(), "{report:?}");
    assert!(
        !report.is_pending(),
        "{selector}: pending={}, other.comment={evidence:?}; {report:?}",
        report.is_pending(),
    );
    assert_eq!(evidence, Some(&Evidence::Leaf(Secrecy::Secret)));
    assert!(report.admitted().is_some());
    assert!(!format!("{report:?}").contains("canary"));
    let resolved = resolve_owner(file, &schemas, &context);
    assert!(resolved.validated_secrecy().is_ok());
}

#[test]
#[ignore = "reader of a parse-time-expanded loop child stays pending: carina#4579"]
fn for_value_keys_reader_literal_list_dot() {
    assert_literal_reader_admitted("item", "['a']", "children.a.comment");
}

#[test]
#[ignore = "reader of a parse-time-expanded loop child stays pending: carina#4579"]
fn for_value_keys_reader_literal_list_subscript() {
    assert_literal_reader_admitted("item", "['a']", "children['a'].comment");
}

#[test]
#[ignore = "reader of a parse-time-expanded loop child stays pending: carina#4579"]
fn for_value_keys_reader_literal_indexed_list() {
    assert_literal_reader_admitted("(i, item)", "['comment']", "children[0].name");
}

#[test]
#[ignore = "reader of a parse-time-expanded loop child stays pending: carina#4579"]
fn for_value_keys_reader_literal_map_attribute_key() {
    assert_literal_reader_admitted("k, v", "({ comment = 'x' })", "children.comment.name");
}

// Assert the current pending state; invert these expectations when carina#4579 is fixed.
#[test]
fn for_value_keys_reader_pending_keeps_runtime_redaction() {
    for selector in ["children.comment.name", "children['comment'].name"] {
        let source = format!(
            "let children = for item in ['comment'] {{ mock.test.Resource {{ comment = secret('canary') name = secret('name canary') }} }}\nlet other = mock.test.Resource {{ comment = {selector} }}\n",
        );
        let (_directory, file, schemas, context) = reader_fixture(&source, None);
        let file = file.unwrap();
        let report = report(&file, &schemas, &context);
        assert!(report.is_pending());
        assert!(report.findings().is_empty());
        assert!(report.admitted().is_none());
        assert!(report.rows.iter().any(|(id, row)| {
            id.identity_str() == Some("other") && !row.attributes.contains_key("comment")
        }));
        let group = file
            .parser_binding_metadata
            .structural_handles()
            .keys()
            .find(|binding| binding.as_str() == "children")
            .expect("authored loop group");
        assert!(file.expression_report().secrecy_definition(group).is_some());
        assert!(file.resources.iter().all(|resource| {
            resource
                .binding
                .as_ref()
                .is_none_or(|binding| binding.as_str() != "children")
        }));
        let resolved = resolve_owner(file, &schemas, &context);
        assert!(resolved.validated_secrecy().is_err());
        let other = resolved
            .as_file()
            .resources
            .iter()
            .find(|resource| resource.id.identity_str() == Some("other"))
            .unwrap();
        let bindings = crate::binding_index::ResolvedBindings::pre_apply_live(
            crate::binding_index::LivePreApplyInputs {
                managed: &resolved.as_file().resources,
                compositions: &[],
                managed_current: &HashMap::new(),
                data_source_values: &crate::data_source_value::DataSourceValues::empty(),
                remote_bindings: &Default::default(),
                wait_aliases: &[],
            },
        );
        let value = crate::validation::deferred_types::with_session_for_test(&context, |session| {
            crate::resolver::resolve_ref_value_with_config(
                &other.attributes["comment"],
                &bindings,
                session,
            )
        })
        .unwrap();
        if selector == "children.comment.name" {
            assert!(
                matches!(
                    value
                        .knowledge(&KnowledgeContext::plan(&bindings))
                        .unwrap()
                        .into_classification(),
                    Classification::Pending(_)
                ),
                "{selector}"
            );
        } else {
            assert!(crate::value::contains_secret(&value), "{selector}");
            assert!(!crate::value::format_value(&value).contains("canary"));
        }
        let child = resolved
            .as_file()
            .resources
            .iter()
            .find(|resource| resource.id.identity_str() == Some("children.comment"))
            .unwrap();
        let provider_values = BTreeMap::from([
            (
                "comment".to_string(),
                Value::Concrete(ConcreteValue::String("canary".into())),
            ),
            (
                "name".to_string(),
                Value::Concrete(ConcreteValue::String("name canary".into())),
            ),
        ]);
        let protected = crate::binding_index::SecretPreservingAttributes::from_provider_values(
            &child.attributes,
            &provider_values,
        );
        let state = crate::resource::State {
            id: child.id.clone(),
            identifier: None,
            exists: true,
            attributes: provider_values,
            dependency_bindings: Default::default(),
            partial_read: None,
        };
        let mut applied = crate::binding_index::ResolvedBindings::default();
        applied.record_applied(
            child.binding.as_ref().map(|binding| binding.as_str()),
            &protected,
            &state,
            crate::binding_index::BindingCompleteness::Complete,
        );
        let value = crate::validation::deferred_types::with_session_for_test(&context, |session| {
            crate::resolver::resolve_ref_value_with_config(
                &other.attributes["comment"],
                &applied,
                session,
            )
        })
        .unwrap();
        if selector == "children.comment.name" {
            assert!(matches!(
                value
                    .knowledge(&KnowledgeContext::apply(&applied))
                    .unwrap()
                    .into_classification(),
                Classification::Pending(_)
            ));
            assert!(!crate::value::format_value(&value).contains("canary"));
        } else {
            assert!(crate::value::contains_secret(&value));
            assert!(!crate::value::format_value(&value).contains("canary"));
            let snapshot = crate::value::redact_secrets_in_value(&value).unwrap();
            assert!(!serde_json::to_string(&snapshot).unwrap().contains("canary"));
        }
    }
}

// Assert the current pending state; invert these expectations when carina#4579 is fixed.
#[test]
fn for_value_keys_reader_pending_mixed_alternatives_characterization() {
    // The rejection of the read under `??` is carina#4580; invert it when fixed.
    let (_directory, file, _schemas, _context) = reader_fixture(
        "let children = for item in ['comment'] { mock.test.Resource { name = secret('name canary') } }\nlet other = mock.test.Resource { comment = children.comment.name ?? 'public' }\n",
        None,
    );
    let error = file.unwrap_err().to_string();
    assert!(
        error.contains("cannot apply access .comment to list(mock.test.Resource)"),
        "{error}"
    );
    assert!(!error.contains("canary"));

    let loop_source = "let children = for item in ['comment'] { mock.test.Resource { comment = secret('canary') name = secret('name canary') } }\nlet other = mock.test.Resource { comment = children.comment.name }\n";
    let (_directory, file, schemas, context) = reader_fixture(loop_source, None);
    let file = file.unwrap();
    let pending = report(&file, &schemas, &context);
    assert!(pending.is_pending());
    assert!(pending.findings().is_empty());
    assert!(
        resolve_owner(file, &schemas, &context)
            .validated_secrecy()
            .is_err()
    );

    for source in [
        "let child = mock.test.Resource { name = secret('name canary') }\nlet other = mock.test.Resource { comment = child.name ?? 'public' }\n".to_string(),
        format!("{loop_source}let independent = mock.test.Resource {{ comment = secret('independent canary') ?? 'public' }}\n"),
    ] {
        let (_directory, file, schemas, context) = reader_fixture(&source, None);
        let file = file.unwrap();
        let report = report(&file, &schemas, &context);
        assert_eq!(report.findings().len(), 1, "{report:?}");
        assert!(matches!(
            report.findings()[0].cause(),
            crate::secrecy::DerivationError::MixedAlternatives(_)
        ));
        let error = crate::validation::deferred_types::with_session_for_test(&context, |session| {
            crate::validation::resolve_bare_binding_uses_in_session(
                file,
                &schemas,
                session,
                &crate::module_resolver::ResolvedModuleSignatures::default(),
            )
        })
        .unwrap_err();
        assert!(matches!(error, crate::validation::ResolveFileError::Secrecy(_)));
        assert!(!format!("{report:?}").contains("canary"));
    }
}

reader_test!(
    issue_4516_reader_map_dot,
    "children.a.comment",
    true,
    false,
    false,
    Pending
);
reader_test!(
    issue_4516_reader_map_subscript,
    "children[\"a\"].comment",
    true,
    false,
    false,
    Pending
);
reader_test!(
    issue_4516_reader_map_attribute_key,
    "children.comment.name",
    true,
    false,
    false,
    Pending
);
reader_test!(
    issue_4516_reader_map_public_attribute_key,
    "children.name.comment",
    true,
    false,
    false,
    Pending
);
reader_test!(
    issue_4516_reader_export_list,
    "children[0].comment",
    false,
    false,
    true,
    Admitted
);

#[test]
fn for_value_keys_reader_export_list() {
    assert_reader_protected_for_iterable(
        "value-keyed list export",
        "children.a.comment",
        ReaderIterable::ValueKeyedList,
        false,
        true,
        true,
        Pending,
    );
}

reader_test!(
    issue_4516_reader_export_map,
    "children.a.comment",
    true,
    false,
    true,
    Pending
);
reader_test!(
    issue_4516_reader_module_list,
    "children[0].comment",
    false,
    true,
    false,
    Rejected(ArgumentValueAccess)
);
reader_test!(
    issue_4516_reader_module_map_dot,
    "children.a.comment",
    true,
    true,
    false,
    Pending
);
reader_test!(
    issue_4516_reader_module_map_subscript,
    "children[\"a\"].comment",
    true,
    true,
    false,
    Rejected(ArgumentValueAccess)
);
reader_test!(
    issue_4516_reader_module_map_attribute_key,
    "children.comment.name",
    true,
    true,
    false,
    Pending
);
reader_test!(
    issue_4516_reader_module_map_public_attribute_key,
    "children.name.comment",
    true,
    true,
    false,
    Pending
);
reader_test!(
    issue_4516_reader_module_export_list,
    "children[0].comment",
    false,
    true,
    true,
    Rejected(ExportsInModule)
);
reader_test!(
    issue_4516_reader_module_export_map,
    "children.a.comment",
    true,
    true,
    true,
    Rejected(ExportsInModule)
);

#[test]
fn issue_4516_reader_export_list_inferred() {
    assert_reader_protected(
        "export list without annotation",
        "children[0].comment",
        false,
        false,
        true,
        false,
        Admitted,
    );
}

#[test]
fn issue_4516_reader_export_map_inferred() {
    assert_reader_protected(
        "export map without annotation",
        "children.a.comment",
        true,
        false,
        true,
        false,
        Pending,
    );
}

#[test]
fn issue_4516_reader_static_export_rejected_secret_export() {
    let (_directory, file, schemas, context) = reader_fixture(
        "let fixed = mock.test.Resource { comment = secret(\"static\") }\nexports { output: String = fixed.comment }\n",
        None,
    );
    let file = file.expect("static export should load without module rejection");
    assert!(file.deferred_for_expressions().is_empty());
    let report = report(&file, &schemas, &context);
    assert!(!report.is_pending());
    assert!(report.findings().is_empty());
    let graph = report
        .admitted()
        .expect("static resource graph is Admitted");
    let fixed = file
        .resources
        .iter()
        .find(|resource| resource.id.identity_str() == Some("fixed"))
        .unwrap();
    assert_eq!(
        graph.attributes(&fixed.id).unwrap()["comment"],
        Evidence::Leaf(Secrecy::Secret)
    );
    let errors = crate::validation::deferred_types::with_session_for_test(&context, |session| {
        crate::validation::check_export_values_for_validation(
            &file.export_params,
            &crate::binding_index::ResolvedBindings::for_expanded_validation(&file),
            session,
        )
        .expect_ordinary_for_test()
    })
    .expect_err("static export: expected Rejected(SecretExport)");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        matches!(errors[0].detail(), ParameterValueFinding::SecretExport { name } if name == "output"),
        "static export: expected Rejected(SecretExport), got {errors:?}"
    );
}
