use std::process::{Command, Output};

fn command(directory: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(directory)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .args(args)
        .output()
        .unwrap()
}
fn rendered(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}
fn write(directory: &std::path::Path, file: &str, source: &str) {
    let path = directory.join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

#[test]
fn round3_no_as_module_loop_plan_uses_the_resolver_root_and_identities() {
    use carina_core::{
        config_loader, module_resolver, parser::ProviderContext, paths::ResolvedPath,
        schema::SchemaRegistry,
    };
    for conditional in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        write(directory.path(), "providers.crn", "provider mock {}\n");
        write(
            directory.path(),
            "backend.crn",
            "backend local { path = 'carina.state.json' }\n",
        );
        write(
            directory.path(),
            "main.crn",
            "let net = use { source = './modules/net' }\nnet { names = ['one', 'two'] }\n",
        );
        let body = if conditional {
            "if true { mock.test.resource { name = 'fixed' } }"
        } else {
            "for n in names { mock.test.resource { name = n } }"
        };
        write(
            directory.path(),
            "modules/net/main.crn",
            &format!("arguments {{ names: list(String) }}\n{body}\n"),
        );
        let root = ResolvedPath::new(directory.path()).unwrap();
        let config = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
        let mut registry = SchemaRegistry::new();
        for schema in carina_provider_mock::builtin_schemas() {
            registry.insert("mock", schema);
        }
        let loaded =
            config_loader::load_configuration_with_config(&root, &config, &registry).unwrap();
        // This is main's resolver entry: its loader retains the configuration root,
        // propagates IO failures, and prepares the module template exactly once.
        let result =
            carina_core::validation::deferred_types::TypeCheckSession::run(&config, |session| {
                module_resolver::resolve_modules_with_diagnostics(
                    &loaded.parsed,
                    &root,
                    session,
                    &registry,
                    &[],
                )
            })
            .publish(&mut Vec::new());
        let mut graph = match result
            .into_validation(&mut carina_core::validation::ConversionValidation::<
                std::convert::Infallible,
            >::default())
        {
            module_resolver::ModuleResolutionDiagnosticResult::Expanded { graph } => graph,
            module_resolver::ModuleResolutionDiagnosticResult::ConstraintFailed {
                findings,
                errors,
            } => panic!(
                "main control constraints: {findings:?}; residuals: {}",
                errors
                    .iter()
                    .map(|e| e.display_relative_to(directory.path()))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            module_resolver::ModuleResolutionDiagnosticResult::Unowned { errors } => panic!(
                "main control errors: {}",
                errors
                    .iter()
                    .map(|e| e.display_relative_to(directory.path()))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        };
        let factories: Vec<Box<dyn carina_core::provider::ProviderFactory>> =
            vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
                Default::default(),
                carina_provider_mock::MockScope::memory(),
            ))];
        let canonical = carina_core::value::canonicalize_resources_with_schemas(
            &mut graph.resources,
            &registry,
        )
        .unwrap();
        carina_core::identifier::compute_anonymous_identifiers_with_factories(
            canonical,
            &graph.providers,
            &factories,
            &registry,
            carina_core::identifier::AnonymousIdentityConflictPolicy::RejectAttributeDerived,
        )
        .unwrap();
        let expected_ids: Vec<_> = graph
            .resources
            .iter()
            .map(|resource| resource.id.to_string())
            .collect();
        let mut control_sources = Vec::new();
        for resource in graph.iter_all_resources() {
            match resource {
                carina_core::parser::ResourceRef::Deferred { deferred, .. } => sources(
                    &serde_json::to_value(deferred.target()).unwrap(),
                    &mut control_sources,
                ),
                carina_core::parser::ResourceRef::Resource(resource) => sources(
                    &serde_json::to_value(&resource.loop_provenance).unwrap(),
                    &mut control_sources,
                ),
                carina_core::parser::ResourceRef::DataSource(_)
                | carina_core::parser::ResourceRef::LiveComposition(_) => {}
            }
        }
        assert!(!control_sources.is_empty());
        let expected: Vec<_> = graph.iter_all_resources().filter(|resource| !matches!(resource, carina_core::parser::ResourceRef::LiveComposition(composition) if composition.binding.is_none())).map(|resource| match resource {
            carina_core::parser::ResourceRef::Deferred { deferred, .. } => format!("{}.<pending> (deferred for at {})", deferred.resource_type(), deferred.target().display_name()),
            carina_core::parser::ResourceRef::Resource(_) | carina_core::parser::ResourceRef::DataSource(_) | carina_core::parser::ResourceRef::LiveComposition(_) => resource.id().to_string(),
        }).collect();
        let validation = command(directory.path(), &["validate", ".", "--json"]);
        let json: serde_json::Value = serde_json::from_slice(&validation.stdout).unwrap();
        assert!(validation.status.success(), "{}", rendered(&validation));
        assert_eq!(json["resources"], serde_json::json!(expected));
        assert!(command(directory.path(), &["init", "."]).status.success());
        let plan = command(
            directory.path(),
            &["plan", "--refresh=false", "--out", "loop.plan.json", "."],
        );
        assert!(plan.status.success(), "{}", rendered(&plan));
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(directory.path().join("loop.plan.json")).unwrap(),
        )
        .unwrap();
        fn sources(value: &serde_json::Value, found: &mut Vec<serde_json::Value>) {
            match value {
                serde_json::Value::Object(fields) => {
                    if fields.contains_key("system_identity") && fields.contains_key("display_file")
                    {
                        found.push(value.clone());
                    }
                    for child in fields.values() {
                        sources(child, found);
                    }
                }
                serde_json::Value::Array(items) => {
                    for child in items {
                        sources(child, found);
                    }
                }
                serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_) => {}
            }
        }
        let mut origins = Vec::new();
        sources(&json, &mut origins);
        assert!(!origins.is_empty(), "{json}");
        for origin in &origins {
            assert_eq!(origin["display_file"], "modules/net/main.crn", "{origin}");
        }
        for expected in control_sources {
            assert!(
                origins.contains(&expected),
                "main resolver loop identity {expected} missing in plan: {origins:?}"
            );
        }
        fn resource_ids(value: &serde_json::Value, ids: &mut std::collections::BTreeSet<String>) {
            match value {
                serde_json::Value::Object(fields) => {
                    if fields.get("provider").is_some_and(|p| p == "mock")
                        && fields
                            .get("resource_type")
                            .is_some_and(|t| t == "test.resource")
                        && fields.contains_key("identity")
                    {
                        let id: carina_core::resource::ResourceId =
                            serde_json::from_value(value.clone()).unwrap();
                        ids.insert(id.to_string());
                    }
                    for child in fields.values() {
                        resource_ids(child, ids);
                    }
                }
                serde_json::Value::Array(items) => {
                    for child in items {
                        resource_ids(child, ids);
                    }
                }
                serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_) => {}
            }
        }
        let mut actual_ids = std::collections::BTreeSet::new();
        resource_ids(&json, &mut actual_ids);
        assert!(!actual_ids.is_empty(), "{json}");
        if conditional {
            assert_eq!(actual_ids.into_iter().collect::<Vec<_>>(), expected_ids);
        } else {
            assert_eq!(
                actual_ids.into_iter().collect::<Vec<_>>(),
                vec![
                    "mock.test.resource.net_e2bc21d899c9bdda.mock_test_resource_72062c09",
                    "mock.test.resource.net_e2bc21d899c9bdda.mock_test_resource_96081a11"
                ]
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn round3_no_as_unreadable_module_file_is_not_skipped() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    write(
        directory.path(),
        "main.crn",
        "provider mock {}\nlet net = use { source = './modules/net' }\nnet { names = ['one', 'two'] }\n",
    );
    write(
        directory.path(),
        "modules/net/main.crn",
        "arguments { names: list(String) }\nfor n in names { mock.test.Thing { name = n } }\n",
    );
    write(
        directory.path(),
        "modules/net/unreadable.crn",
        "# must not be skipped\n",
    );
    let file = directory.path().join("modules/net/unreadable.crn");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o0)).unwrap();
    let output = command(directory.path(), &["validate", ".", "--json"]);
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!output.status.success(), "{}", rendered(&output));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["errors"].as_array().unwrap().len(), 1, "{json}");
    assert!(
        json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("Permission denied"),
        "{json}"
    );
}

#[test]
fn round3_read_only_commands_accept_the_same_config_with_a_conversion() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let wasm = ["carina_provider_mock.wasm", "carina-provider-mock.wasm"]
        .into_iter()
        .map(|name| workspace.join("target/wasm32-wasip2/debug").join(name))
        .find(|path| path.is_file())
        .expect("build the mock WASM fixture");
    let source = format!("file://{}", wasm.display());
    write(
        directory.path(),
        "providers.crn",
        &format!("provider mock {{ source = '{source}' }}\n"),
    );
    write(
        directory.path(),
        "backend.crn",
        "backend local { path = 'carina.state.json' }\n",
    );
    assert!(
        command(directory.path(), &["init", ".", "--accept-egress", &source])
            .status
            .success()
    );
    for args in [
        vec!["destroy", "--auto-approve", "."],
        vec!["state", "refresh", "."],
    ] {
        let mut outcomes = Vec::new();
        for expression in [
            "mock.actions.OidcSubject.ClaimKey.repo",
            "mock.actions.OidcSubject.ClaimKey.repo as mock.actions.OidcSubject.ClaimKey",
        ] {
            write(
                directory.path(),
                "main.crn",
                &format!(
                    "let subject = mock.actions.OidcSubject {{ claim_keys = [{expression}] }}\n"
                ),
            );
            let validated = command(directory.path(), &["validate", ".", "--json"]);
            assert!(
                validated.status.success(),
                "the read-only fixture must be valid: {}",
                rendered(&validated)
            );
            let output = command(directory.path(), &args);
            outcomes.push((output.status.code(), rendered(&output)));
        }
        assert_eq!(outcomes[0], outcomes[1], "{args:?}");
        assert_eq!(outcomes[1].0, Some(0), "{outcomes:?}");
    }
}

#[test]
fn round3_editor_grammars_highlight_as_and_stay_identical() {
    let vscode = include_str!("../../editors/vscode/syntaxes/carina.tmLanguage.json");
    let textmate = include_str!("../../editors/carina.tmbundle/Syntaxes/carina.tmLanguage.json");
    assert_eq!(vscode, textmate);
    let json: serde_json::Value = serde_json::from_str(vscode).unwrap();
    assert!(json.to_string().contains("for|in|if|else|as"));
}
