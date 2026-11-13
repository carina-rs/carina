//! Correct-behavior regressions for the first slice of carina#4301.

use std::{path::Path, sync::Arc};

use carina_core::{
    EnvironmentSnapshot,
    config_loader::{
        load_configuration, load_configuration_for_saved_rehydration, parse_directory,
    },
    module_resolver::resolve_modules_with_providerless_config,
    parser::ProviderContext,
    paths::ResolvedPath,
    resource::{
        ConcreteValue, DeferredValue, ModuleSource, ParserPlaceholder, Resource, Value, ValueKind,
    },
    schema::{
        AttributeSchema, AttributeType, ResourceSchema, SchemaRegistry, StructField, TypeIdentity,
    },
};

fn text<P: ValueKind>(value: &str) -> Value<P> {
    Value::Concrete(ConcreteValue::String(value.into()))
}

fn secret(value: Value) -> Value {
    Value::Deferred(DeferredValue::Secret(Box::new(value).into()))
}

fn fixture(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, body) in files {
        let path = dir.path().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    dir
}

#[test]
fn issue_2791_secret_validation_matrix() {
    let custom = AttributeType::refined_string_with_validator(
        TypeIdentity::bare("Rejected"),
        None,
        None,
        carina_core::schema::validator(|_| {
            Err(carina_core::schema::TypeError::ValidationFailed {
                message: "private-validation-message".into(),
            })
        }),
        None,
    );
    let cases = [
        (
            "primitive",
            AttributeType::int(),
            text("private-invalid-int"),
        ),
        (
            "enum",
            AttributeType::enum_(
                TypeIdentity::bare("Choice"),
                Some(vec!["ok".into()]),
                vec![],
                None,
                None,
            ),
            text("private-invalid-enum"),
        ),
        ("custom", custom, text("private-custom")),
        (
            "list",
            AttributeType::list(AttributeType::int()),
            Value::Concrete(ConcreteValue::List(vec![secret(text("private-item"))])),
        ),
        (
            "map",
            AttributeType::map(AttributeType::int()),
            Value::Concrete(ConcreteValue::Map(
                indexmap::indexmap! { "key".into() => secret(text("private-entry")) },
            )),
        ),
        (
            "struct",
            AttributeType::struct_(
                "Record",
                vec![StructField::required("count", AttributeType::int()).build()],
            ),
            Value::Concrete(ConcreteValue::Map(
                indexmap::indexmap! { "count".into() => secret(text("private-field")) },
            )),
        ),
    ];
    let mut failures = Vec::new();
    for (label, ty, invalid) in cases {
        let schema = ResourceSchema::new("test.Record")
            .attribute(AttributeSchema::required("value", ty).build())
            .validate()
            .unwrap();
        let attribute = schema.effective_attribute("value").unwrap();
        for (position, value) in [("inner", invalid.clone()), ("whole", secret(invalid))] {
            match attribute.validate(&value) {
                Ok(()) => failures.push(format!("{label}/{position}: invalid value accepted")),
                Err(error) => {
                    let rendered = format!("{error:?} {error}");
                    if (value_is_secret(&value) || matches!(label, "list" | "map" | "struct"))
                        && rendered.contains("private-")
                    {
                        failures.push(format!("{label}/{position}: protected diagnostic leaked"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

fn value_is_secret(value: &Value) -> bool {
    matches!(value, Value::Deferred(DeferredValue::Secret(_)))
}

fn expanded_resources(
    dir: &Path,
) -> Result<Vec<Resource<ModuleSource, ParserPlaceholder>>, String> {
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let path = ResolvedPath::new(dir).unwrap();
    let parsed = parse_directory(&path, &context)?;
    carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
        resolve_modules_with_providerless_config(parsed, &path, session, &SchemaRegistry::new())
            .map(|file| file.resources)
            .map_err(|error| error.to_string())
    })
}

#[test]
fn issue_2715_secret_function_parameter_and_local_reference_substitution() {
    let dir = fixture(&[
        ("providers.crn", "provider mock {}\n"),
        (
            "functions.crn",
            "fn protect(input: String): String {\n let local = input\n secret(local)\n}\n",
        ),
        (
            "main.crn",
            "let value = \"private-function-input\"\nlet target = mock.test.resource { name = \"target\" comment = protect(value) }\n",
        ),
    ]);
    let resources =
        expanded_resources(dir.path()).expect("function parameters and locals substitute");
    let value = resources[0].get_attr("comment").expect("comment");
    let Value::Deferred(DeferredValue::Secret(payload)) = value else {
        panic!("function substitution must preserve Secret: {value:?}");
    };
    assert_eq!(
        payload.plaintext_for_test(),
        &text("private-function-input")
    );
    assert!(!format!("{value:?}").contains("private-function-input"));
}

#[test]
fn issue_2715_nested_secret_module_argument_interpolation_and_local_refs() {
    let mut failures = Vec::new();
    for (label, argument) in [
        ("direct", "secret(password)"),
        ("interpolation", "secret(\"${password}\")"),
    ] {
        let outer = format!(
            "let inner = use {{ source = \"../inner\" }}\nlet nested = inner {{ password = {argument} }}\n"
        );
        let dir = fixture(&[
            ("providers.crn", "provider mock {}\n"),
            (
                "main.crn",
                "let outer = use { source = \"./outer\" }\nlet instance = outer { password = \"private-module-input\" }\n",
            ),
            ("outer/main.crn", "arguments { password: String }\n"),
            ("outer/nested.crn", &outer),
            ("inner/main.crn", "arguments { password: String }\n"),
            (
                "inner/role.crn",
                "let role = mock.test.resource { name = \"role\" }\n",
            ),
            (
                "inner/consumer.crn",
                "let consumer = mock.test.resource { name = \"consumer\" comment = secret(\"prefix-${password}\") replacement_key = secret(role.identifier) }\n",
            ),
        ]);
        let resources = match expanded_resources(dir.path()) {
            Ok(resources) => resources,
            Err(error) => {
                failures.push(format!("{label}: {error}"));
                continue;
            }
        };
        let role = resources
            .iter()
            .find(|r| r.get_attr("name") == Some(&text("role")))
            .unwrap();
        let consumer = resources
            .iter()
            .find(|r| r.get_attr("name") == Some(&text("consumer")))
            .unwrap();
        let Value::Deferred(DeferredValue::Secret(payload)) = consumer.get_attr("comment").unwrap()
        else {
            panic!("interpolation lost Secret")
        };
        assert_eq!(
            payload.plaintext_for_test(),
            &text("prefix-private-module-input")
        );
        let Value::Deferred(DeferredValue::Secret(payload)) =
            consumer.get_attr("replacement_key").unwrap()
        else {
            panic!("local reference lost Secret")
        };
        let Value::Deferred(DeferredValue::ResourceRef { path }) = payload.plaintext_for_test()
        else {
            panic!("local reference must remain a qualified provider reference")
        };
        assert_eq!(path.binding_address(), role.binding.as_ref().unwrap());
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn issue_3979_validate_strict_rejects_secret_missing_env_and_decrypt() {
    let mut failures = Vec::new();
    for (label, expression) in [
        ("env", "env(\"CARINA_4301_MISSING\")"),
        ("decrypt", "decrypt(\"private-ciphertext\")"),
    ] {
        for protected in [false, true] {
            let expression = if protected {
                format!("secret({expression})")
            } else {
                expression.into()
            };
            let source = format!(
                "let target = mock.test.resource {{ name = \"target\" comment = {expression} }}\n"
            );
            let dir = fixture(&[
                ("main.crn", &source),
                ("providers.crn", "provider mock {}\n"),
            ]);
            let mut context = ProviderContext::new(EnvironmentSnapshot::empty());
            context.decryptor = Some(Arc::new(|_, _| Err("private-decrypt-error".into())));
            match load_configuration(&ResolvedPath::new(dir.path()).unwrap(), &context) {
                Ok(_) => failures.push(format!(
                    "{label}/secret={protected}: strict validation accepted failing builtin"
                )),
                Err(error) if protected && error.contains("private-") => {
                    failures.push(format!("{label}: protected failure leaked"))
                }
                Err(_) => {}
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[test]
fn issue_3979_saved_rehydration_retains_unavailable_candidates() {
    for expression in [
        "env(\"private-missing-env\")",
        "decrypt(\"private-ciphertext\")",
    ] {
        for protected in [false, true] {
            let expression = if protected {
                format!("secret({expression})")
            } else {
                expression.into()
            };
            let dir = fixture(&[
                ("providers.crn", "provider mock {}\n"),
                ("value.crn", &format!("let token = {expression}\n")),
            ]);
            let mut context = ProviderContext::new(EnvironmentSnapshot::empty());
            context.decryptor = Some(Arc::new(|_, _| Err("private-decrypt-error".into())));
            let loaded = load_configuration_for_saved_rehydration(
                &ResolvedPath::new(dir.path()).unwrap(),
                &context,
                &SchemaRegistry::new(),
            )
            .expect("rehydration must retain failed candidate evidence regardless of protection");
            let token = &loaded.parsed.variables["token"];
            let unavailable = if let Value::Deferred(DeferredValue::Secret(payload)) = token {
                payload.plaintext_for_test()
            } else {
                token
            };
            assert!(matches!(
                unavailable,
                Value::Deferred(DeferredValue::Unavailable { .. })
            ));
            if protected {
                assert!(!format!("{token:?}").contains("private-"));
            }
        }
    }
}

#[test]
fn issue_3979_retained_coalesce_does_not_execute_during_strict_resolution() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = fixture(&[
        ("providers.crn", "provider mock {}\n"),
        (
            "main.crn",
            "let origin = mock.test.resource { name = \"origin\" }\nlet target = mock.test.resource { name = \"target\" comment = origin.comment ?? secret(decrypt(origin.identifier)) }\n",
        ),
    ]);
    let calls = Arc::new(AtomicUsize::new(0));
    let decrypt_calls = Arc::clone(&calls);
    let mut context = ProviderContext::new(EnvironmentSnapshot::empty());
    context.decryptor = Some(Arc::new(move |_, _| {
        decrypt_calls.fetch_add(1, Ordering::SeqCst);
        Err("private-decrypt-error".into())
    }));
    let loaded = load_configuration(&ResolvedPath::new(dir.path()).unwrap(), &context)
        .expect("a retained fallback is not required yet");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(matches!(
        loaded.parsed.resources[1].get_attr("comment"),
        Some(Value::Deferred(DeferredValue::Coalesce { .. }))
    ));
}

#[test]
fn bootstrap_and_saved_loading_only_demand_consumed_backend_values() {
    let directory = fixture(&[
        (
            "main.crn",
            "let item = mock.test.resource { name = 'item' }\n",
        ),
        (
            "providers.crn",
            "provider mock {}\nlet token = secret(env('UNSET_X'))\nlet unused = env('UNSET_Y')\n",
        ),
        ("backend.crn", "backend local { path = 'state.json' }\n"),
    ]);
    let path = ResolvedPath::new(directory.path()).unwrap();
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    for loader in [
        carina_core::config_loader::load_configuration_for_bootstrap,
        saved_load_with_rendered_error,
    ] {
        let loaded = loader(&path, &context, &SchemaRegistry::new()).unwrap();
        assert!(matches!(
            loaded.parsed.variables.get("token"),
            Some(Value::Deferred(DeferredValue::Secret(_)))
        ));
        assert!(matches!(
            loaded.parsed.variables.get("unused"),
            Some(Value::Deferred(DeferredValue::Unavailable { .. }))
        ));
    }
}

#[test]
fn saved_loading_rejects_unavailable_backend_values_with_location() {
    let directory = fixture(&[
        (
            "main.crn",
            "let item = mock.test.resource { name = 'item' }\n",
        ),
        ("providers.crn", "provider mock {}\n"),
        ("backend.crn", "backend s3 { bucket = env('MISSING') }\n"),
    ]);
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let error = carina_core::config_loader::load_configuration_for_saved_rehydration(
        &ResolvedPath::new(directory.path()).unwrap(),
        &context,
        &SchemaRegistry::new(),
    )
    .err()
    .unwrap();
    let error = error.to_string();
    assert!(error.contains("backend.crn:1:"), "{error}");
    assert!(
        error.contains("environment variable 'MISSING' is not set"),
        "{error}"
    );
}

#[test]
fn metadata_backend_demands_its_let_dependency_but_not_unrelated_lets() {
    let directory = fixture(&[
        (
            "main.crn",
            "let item = mock.test.resource { name = 'item' }\n",
        ),
        (
            "providers.crn",
            "provider mock {}\nlet token = env('UNSET_X')\nlet unrelated = secret(env('UNSET_Y'))\n",
        ),
        ("backend.crn", "backend local { path = token }\n"),
    ]);
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    for loader in [
        carina_core::config_loader::load_configuration_for_bootstrap,
        saved_load_with_rendered_error,
    ] {
        let error = loader(
            &ResolvedPath::new(directory.path()).unwrap(),
            &context,
            &SchemaRegistry::new(),
        )
        .err()
        .unwrap();
        assert!(error.contains("backend.crn:1:"), "{error}");
        assert!(
            error.contains("environment variable 'UNSET_X' is not set"),
            "{error}"
        );
        assert!(!error.contains("UNSET_Y"), "{error}");
    }
}

#[test]
fn r5_bootstrap_phase_survives_providerless_module_resolution() {
    let dir = fixture(&[
        (
            "main.crn",
            "let child = use { source = './child' }\nlet instance = child {}\n",
        ),
        (
            "child/main.crn",
            "arguments { name: String = 'safe' }\nlet unused = secret(env('R5_MODULE_UNSET'))\nlet item = mock.test.resource { name = name }\n",
        ),
    ]);
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let path = ResolvedPath::new(dir.path()).unwrap();
    let loaded =
        carina_core::config_loader::parse_directory_for_bootstrap(&path, &context).unwrap();
    let expanded =
        carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
            resolve_modules_with_providerless_config(loaded, &path, session, &SchemaRegistry::new())
        })
        .unwrap();
    assert_eq!(expanded.resources.len(), 1);
}

#[test]
fn r5_saved_loader_classifies_failed_backend_roots() {
    use carina_core::config_loader::ConfigurationLoadFailureKind;
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    for expression in [
        "env('R5_BACKEND_UNSET')",
        "['safe'][7]",
        "secret(env('R5_BACKEND_UNSET'))",
    ] {
        let dir = fixture(&[(
            "backend.crn",
            &format!("backend local {{ path = {expression} }}\n"),
        )]);
        let error = load_configuration_for_saved_rehydration(
            &ResolvedPath::new(dir.path()).unwrap(),
            &context,
            &SchemaRegistry::new(),
        )
        .err()
        .unwrap();
        assert_eq!(
            error.kind(),
            ConfigurationLoadFailureKind::BackendInput,
            "{expression}: {error}"
        );
        assert!(error.to_string().contains("backend.crn"), "{error}");
    }
}

fn saved_load_with_rendered_error(
    path: &ResolvedPath,
    context: &ProviderContext,
    schemas: &SchemaRegistry,
) -> Result<carina_core::config_loader::LoadedConfig, String> {
    load_configuration_for_saved_rehydration(path, context, schemas)
        .map_err(|error| error.to_string())
}

#[test]
fn r5_metadata_discovery_leaves_unconsumed_secret_backend_deferred() {
    let directory = fixture(&[(
        "backend.crn",
        "backend local { path = secret(env('R5_UNUSED_BACKEND')) }\n",
    )]);
    let path = ResolvedPath::new(directory.path()).unwrap();
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let schemas = SchemaRegistry::new();
    assert!(
        carina_core::config_loader::load_configuration_for_metadata(&path, &context, &schemas)
            .unwrap()
            .parsed
            .backend
            .is_some()
    );
    assert!(
        carina_core::config_loader::parse_directory_for_metadata(
            &path,
            &context,
            &indexmap::IndexMap::new(),
            &schemas
        )
        .unwrap()
        .backend
        .is_some()
    );
    let error =
        carina_core::config_loader::load_configuration_for_bootstrap(&path, &context, &schemas)
            .err()
            .unwrap();
    assert!(error.contains("secret()- and decrypt()-derived values are not allowed"));
    assert!(!error.contains("R5_UNUSED_BACKEND"));
}

#[test]
fn r5_upstream_export_discovery_retains_unrelated_failed_inputs() {
    let directory = fixture(&[
        ("main.crn", "// downstream\n"),
        (
            "upstream/main.crn",
            "let unused = secret(env('R5_EXPORT_UNSET'))\nexports { label: String = 'visible' }\n",
        ),
    ]);
    let context = ProviderContext::new(EnvironmentSnapshot::empty());
    let source = carina_core::parser::UpstreamState {
        binding: carina_core::resource::RelativeBindingName::new("upstream"),
        source: "./upstream".into(),
    };
    let exports =
        carina_core::validation::deferred_types::with_session_for_test(&context, |session| {
            carina_core::upstream_exports::discover_upstream_exports(
                &ResolvedPath::new(directory.path()).unwrap(),
                &[source],
                session,
                Some(&SchemaRegistry::new()),
            )
        });
    assert!(
        exports
            .get(&carina_core::resource::RelativeBindingName::new("upstream"))
            .is_some_and(|entries| entries.contains_key("label")),
        "{exports:?}"
    );
}
