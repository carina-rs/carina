use super::*;

use crate::module_resolver::ModuleError;
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
    let header = if map { "key, item" } else { "item" };
    // The colliding map key selects `name`, so make that attribute secret too.
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
    let module_source = format!(
        "arguments {{ items: {}(String) }}\n{body}",
        if map { "map" } else { "list" }
    );
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
