use super::*;
pub(super) const SECRET_PROBE: &str = "declared-secret-cobalt-784531";

pub(super) fn assert_no_secret(text: &str, surface: &str) {
    assert!(
        !text.contains(SECRET_PROBE),
        "SECRET_LEAK_DETECTED surface={surface}"
    );
}
pub(super) fn secret_case(interpolation: bool) -> Case {
    let mut case = fixture_case("c2-module-upstream-param-identity-late-invalid");
    case.child = Some(if interpolation {
        format!(
            "arguments {{ label: String }}\nlet src = test.Thing {{ name = 'src' plain = secret('{SECRET_PROBE}') }}\nfn g(r: test.Thing): test.Name {{ \"prefix-${{r.plain}}\" }}\ntest.Sink {{ name = 'sink' value = g(src) }}\n"
        )
    } else {
        format!(
            "arguments {{ label: String }}\nlet src = test.Thing {{ name = 'src' plain = secret('{SECRET_PROBE}') }}\nfn g(x: test.Name): String {{ x }}\ntest.Sink {{ name = 'sink' value = g(src.plain) }}\n"
        )
    });
    case
}

async fn secret_commands(save: bool) {
    let mut all_rejected = true;
    for interpolation in [false, true] {
        for apply in [false, true] {
            let directory = Directory::new(&secret_case(interpolation));
            let saved = directory.path().join("secret.plan.json");

            let result = if apply {
                directory.apply().await.map(|()| false)
            } else {
                directory.plan(if save { Some(&saved) } else { None }).await
            };
            let diagnostic = rendered(&result);

            assert_no_secret(&diagnostic, "diagnostic");
            for (file, surface) in [
                (directory.path().join("state.json"), "state"),
                (saved, "plan"),
            ] {
                if file.exists() {
                    assert_no_secret(&std::fs::read_to_string(file).unwrap(), surface);
                }
            }
            if save && !apply {
                assert!(result.is_err());
                assert!(!directory.path().join("secret.plan.json").exists());
                let key = if interpolation {
                    "secret-interpolation-control"
                } else {
                    "secret-parameter-control"
                };
                assert_eq!(diagnostic, expected_saved_plan_error(key, &directory));
            }
            all_rejected &= result.is_err();
            if result.is_err() {
                assert!(
                    diagnostic.contains("got secret")
                        || diagnostic.contains(carina_core::resource::INSIDE_SECRET_MESSAGE),
                    "{diagnostic}"
                );
            }
            if result.is_err() {
                assert_eq!(directory.fixture.count("create", "Sink"), 0);
            }
            directory.assert_unlocked().await;
        }
    }
    assert!(all_rejected, "secret annotation must reject");
}

#[tokio::test]
async fn secret_parameter_and_interpolation() {
    secret_commands(false).await;
}
#[tokio::test]
async fn secret_parameter_and_interpolation_plan_out() {
    secret_commands(true).await;
}
async fn unused_handle_secret(computed: bool) {
    let mut case = fixture_case("c1-handle-late-invalid");
    case.emitted = "allowed".into();
    let db = if computed {
        "let db = test.Thing { name = 'db' }\n".to_owned()
    } else {
        format!("let db = test.Thing {{ name = 'db' plain = secret('{SECRET_PROBE}') }}\n")
    };
    case.main = db.clone();
    let directory = Directory::new(&case);
    if computed {
        *directory.fixture.private.lock().unwrap() = Some(SECRET_PROBE.into());
    }

    let seeded = directory.apply().await;
    let seeded_error = rendered(&seeded);

    assert_no_secret(&seeded_error, "diagnostic");
    let state = std::fs::read_to_string(directory.path().join("state.json")).unwrap();
    assert_no_secret(&state, "state");
    assert!(seeded.is_ok(), "{seeded_error}");
    assert!(
        state.contains(if computed { "private" } else { "plain" }) && state.contains("$secret"),
        "sensitive output must persist only as a hash"
    );
    std::fs::write(directory.path().join("main.crn"), format!("{db}let other = test.Thing {{ name = 'other' }}\nfn f(r: test.Thing, x: test.Name): String {{ x }}\ntest.Sink {{ name = 'sink' value = f(db, other.emitted) }}\n")).unwrap();

    let planned = directory.plan(None).await;
    let applied = directory.apply().await;
    let plan_error = rendered(&planned);
    let apply_error = rendered(&applied);

    for (text, surface) in [(&plan_error, "diagnostic"), (&apply_error, "diagnostic")] {
        assert_no_secret(text, surface);
    }
    assert_no_secret(
        &std::fs::read_to_string(directory.path().join("state.json")).unwrap(),
        "state",
    );
    assert!(planned.is_ok(), "{plan_error}");
    assert!(applied.is_ok(), "{apply_error}");
    assert_eq!(directory.fixture.count("create", "Sink"), 1);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn unused_handle_with_historical_secret() {
    unused_handle_secret(false).await;
}
#[tokio::test]
async fn unused_handle_with_historical_secret_computed() {
    unused_handle_secret(true).await;
}

#[tokio::test]
async fn late_rejection_refresh_and_destroy() {
    for name in [
        "c2-module-upstream-param-identity-late-invalid",
        "c2-module-handle-param-const-late-invalid",
        "c1-handle-late-invalid",
    ] {
        let directory = Directory::new(&fixture_case(name));
        let rejected = directory.apply().await;
        directory.assert_unlocked().await;
        let state = std::fs::read_to_string(directory.path().join("state.json")).unwrap();
        assert!(state.contains("bad-name"));
        let planned = directory.plan(None).await;
        directory.assert_unlocked().await;
        let refreshed = directory.refresh().await;
        let destroyed = directory.destroy().await;
        assert!(rejected.is_err(), "late apply must reject");
        assert!(
            planned.is_err(),
            "plan against persisted bad value must reject"
        );
        assert!(refreshed.is_ok(), "{}", rendered(&refreshed));
        assert!(destroyed.is_ok(), "{}", rendered(&destroyed));
        assert_eq!(directory.fixture.count("create", "Thing"), 1);
        assert_eq!(directory.fixture.count("create", "Sink"), 0);
        assert_eq!(directory.fixture.count("delete", "Thing"), 1);
        let state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(directory.path().join("state.json")).unwrap())
                .unwrap();
        assert!(state["resources"].as_array().unwrap().is_empty());
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn retained_module_attribute() {
    for valid in [true, false] {
        let mut case = fixture_case("c2-module-upstream-param-identity-late-invalid");
        case.main
            .push_str("test.Sink { name = 'sink' value = instance.output }\n");
        case.child = Some("arguments { label: String }\nlet src = test.Thing { name = 'src' }\nfn g(x: test.Name): String { x }\nattributes { output: String = g(src.emitted) }\n".into());
        case.emitted = if valid { "allowed" } else { "bad-name" }.into();
        let directory = Directory::new(&case);
        let planned = directory.plan(None).await;
        let applied = directory.apply().await;
        assert!(planned.is_ok(), "{}", rendered(&planned));
        assert_eq!(applied.is_ok(), valid, "{}", rendered(&applied));
        assert_eq!(
            directory.fixture.count("create", "Sink"),
            usize::from(valid)
        );
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn nested_call_error_has_one_prefix() {
    for module in [false, true] {
        let mut case = fixture_case("c1-handle-late-invalid");
        let body = "let src = test.Thing { name = 'src' }\nfn inner(x: test.Name): String { x }\nfn outer(x) { 'ignored' }\ntest.Sink { name = 'sink' value = outer(inner(src.emitted)) }\n";
        let line = if module {
            case.main = "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n".into();
            case.child = Some(format!("arguments {{ label: String }}\n{body}"));
            5
        } else {
            case.main = body.into();
            4
        };
        let directory = Directory::new(&case);
        let result = directory.apply().await;
        let diagnostic = rendered(&result);
        assert_eq!(
            diagnostic,
            format!("Invalid expression at line {line}: function 'inner': parameter 'x': {SUFFIX}")
        );
        assert_eq!(directory.fixture.count("create", "Sink"), 0);
    }
}

#[tokio::test]
async fn retained_plan_json() {
    for (name, expected) in [
        (
            "c1-handle-late-valid",
            "A saved plan cannot carry this call.",
        ),
        (
            "c2-module-param-const-literal-invalid",
            "expected test.Name",
        ),
    ] {
        let directory = Directory::new(&fixture_case(name));

        let result = directory.plan_output(None, true).await;
        let diagnostic = rendered(&result);

        assert_no_secret(&diagnostic, "JSON diagnostic");

        assert!(
            result.is_err(),
            "retained/rejected plans have no JSON artifact"
        );
        assert!(diagnostic.contains(expected), "{diagnostic}");
    }
}

#[tokio::test]
async fn non_resource_handle_arguments() {
    for (kind, declaration) in [
        ("Use", "let handle = use { source = './child' }"),
        (
            "Module",
            "let child = use { source = './child' }\nlet handle = child {}",
        ),
        ("Provider", "let handle = provider test {}"),
        (
            "For",
            "let handle = for n in ['row'] { test.Thing { name = n } }",
        ),
        ("If", "let handle = if true { test.Thing { name = 'row' } }"),
        ("UpstreamState", ""),
    ] {
        let upstream = Directory::new(&Case {
            name: "upstream".into(),
            main: "exports { output: String = 'allowed' }\n".into(),
            child: None,
            emitted: "allowed".into(),
        });
        upstream.apply().await.unwrap();
        let declaration = if kind == "UpstreamState" {
            format!(
                "let handle = upstream_state {{ source = '{}' }}",
                upstream.path().display()
            )
        } else {
            declaration.into()
        };
        let mut case = fixture_case("c1-handle-late-invalid");
        case.main = format!(
            "{declaration}\nlet src = test.Thing {{ name = 'src' }}\nfn f(r, x: test.Name): String {{ x }}\ntest.Sink {{ name = 'sink' value = f(handle, src.emitted) }}\n"
        );
        case.child = Some("attributes { output: String = 'allowed' }\n".into());
        let directory = Directory::new(&case);
        let plan = directory.plan(None).await;
        let apply = directory.apply().await;
        match &plan {
            Ok(_) => assert_declared_error(&rendered(&apply), "function 'f': parameter 'x':"),
            Err(_) => {
                assert!(
                    rendered(&plan).contains("dependency target 'handle' does not exist"),
                    "{kind}: {}",
                    rendered(&plan)
                );
                assert!(
                    rendered(&apply).contains("dependency target 'handle' does not exist"),
                    "{kind}: {}",
                    rendered(&apply)
                );
            }
        }
        assert_eq!(directory.fixture.count("create", "Sink"), 0);
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn retained_export_consumed_upstream() {
    for valid in [true, false] {
        let mut case = fixture_case("c1-handle-late-invalid");
        case.main = "let src = test.Thing { name = 'src' }\nfn g(x: test.Name): String { x }\nexports { output: String = g(src.emitted) }\n".into();
        case.emitted = if valid { "allowed" } else { "bad-name" }.into();
        let producer = Directory::new(&case);
        let planned = producer.plan(None).await;
        let produced = producer.apply().await;
        assert!(planned.is_ok(), "{}", rendered(&planned));
        let mut consumer_case = case.clone();
        consumer_case.main = format!(
            "let up = upstream_state {{ source = '{}' }}\ntest.Sink {{ name = 'consumer' value = up.output }}\n",
            producer.path().display()
        );
        let consumer = Directory::new(&consumer_case);
        let consumed = consumer.apply().await;
        assert_eq!(produced.is_ok(), valid, "{}", rendered(&produced));
        assert_eq!(consumed.is_ok(), valid, "{}", rendered(&consumed));
        assert_eq!(consumer.fixture.count("create", "Sink"), usize::from(valid));
    }
}

#[test]
fn unloaded_provider_validate_diagnostics_and_hover() {
    use carina_lsp::{document::Document, hover::HoverProvider, test_support::Diagnostics};
    let case = Case {
        name: "provider-unloaded".into(),
        main: "fn g(x: test.Name): String { 'allowed' }\nlet answer = g('allowed')\n".into(),
        child: None,
        emitted: "allowed".into(),
    };
    let directory = Directory::new(&case);
    let validation = commands::validate::validate_diagnostics_with_factories(
        &ResolvedPath::new(directory.path()).unwrap(),
        Vec::new(),
        HashMap::from([("test".into(), "fixture schemas unavailable".into())]),
    );
    let context = Arc::new(directory.context());
    let file = directory.path().join("main.crn");
    let document = Document::new_with_path(case.main, file.clone(), context);
    let diagnostics = Diagnostics::from_schemas(directory.path(), SchemaRegistry::new())
        .with_load_errors(BTreeMap::from([(
            "test".into(),
            "fixture schemas unavailable".into(),
        )]))
        .analyze(&document, Some(&file));
    let hover = HoverProvider::new(Arc::new(SchemaRegistry::new()), vec![]).hover_with_context(
        &document,
        tower_lsp::lsp_types::Position::new(1, 6),
        Some(directory.path()),
        Some("main.crn"),
    );
    let message = "function 'g' parameter 'x': type check for test.Name was deferred because provider schemas were not loaded";
    assert_eq!(validation.warnings.len(), 2);
    assert!(
        validation
            .warnings
            .iter()
            .all(|warning| warning.message == message)
    );
    assert_eq!(diagnostics.len(), 2);
    assert!(
        diagnostics
            .iter()
            .all(|diagnostic| diagnostic.message == message)
    );
    assert_eq!(
        diagnostics[0].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(0, 8),
            tower_lsp::lsp_types::Position::new(0, 17)
        )
    );
    assert_eq!(
        diagnostics[1].range,
        tower_lsp::lsp_types::Range::new(
            tower_lsp::lsp_types::Position::new(1, 13),
            tower_lsp::lsp_types::Position::new(1, 25)
        )
    );
    assert!(hover.is_none());
}

#[tokio::test]
async fn return_annotation_resolution_is_declaration_owned() {
    for source in [
        "fn f(): test.Missing { 'allowed' }\ntest.Sink { name = 'sink' value = f() }\n",
        "let src = test.Thing { name = 'src' }\nfn f(r: test.Thing): test.Thing { r }\ntest.Sink { name = 'sink' value = f(src) }\n",
    ] {
        let directory = Directory::new(&Case {
            name: "return-annotation".into(),
            main: source.into(),
            child: None,
            emitted: "allowed".into(),
        });
        let validation = directory.validate();
        let planned = directory.plan(None).await;
        assert_ne!(validation.exit_code(), 0);
        assert!(planned.is_err());
    }
}
#[tokio::test]
async fn return_reference_schema_mismatch_precedes_shadow_recheck() {
    let case = Case { name: "shadowed-parameter".into(), main: "let src = test.Thing { name = 'src' }\nfn g(x: 'allowed'): test.Name { let x = 'bad'\nsrc.emitted }\ntest.Sink { name = 'sink' value = g('allowed') }\n".into(), child: None, emitted: "allowed".into() };
    let directory = Directory::new(&case);
    let validation = directory.validate();
    let plan = directory.plan(None).await;
    let apply = directory.apply().await;
    let message = "function 'g': return value: reference src.emitted: expected test.Name, got String (from src.emitted)";
    assert_eq!(validation.exit_code(), 1);
    let diagnostics = validation.diagnostic_output_for_test();
    assert_eq!(diagnostics["errors"].as_array().unwrap().len(), 1);
    assert_eq!(diagnostics["errors"][0]["message"], message);
    let expected = format!("Module resolution error: Reference type validation failed: {message}");
    assert_eq!(rendered(&plan), expected);
    assert_eq!(rendered(&apply), expected);
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    directory.assert_unlocked().await;
}
#[tokio::test]
async fn primitive_module_argument_reaches_runtime_saved_plan_control() {
    let mut case = fixture_case("c2-module-param-identity-late-invalid");
    case.child = Some("arguments { input: String }\nfn g(x: String): String { x }\ntest.Sink { name = 'sink' value = g(input) }\n".into());
    let directory = Directory::new(&case);
    let parsed = carina_core::parser::parse_file(
        case.child.as_ref().unwrap(),
        &directory.path().join("child/main.crn"),
        &directory.context(),
    )
    .unwrap();
    assert!(matches!(
        parsed.resources[0].get_attr("value"),
        Some(carina_core::resource::Value::Deferred(
            carina_core::resource::DeferredValue::ResolvedFunctionCall { .. }
        ))
    ));
    let saved = directory.path().join("primitive.plan.json");
    let planned = directory.plan(Some(&saved)).await;
    assert!(planned.is_ok(), "{}", rendered(&planned));
    assert!(saved.exists());
    directory.apply_saved(&saved).await.unwrap();
    assert_eq!(directory.fixture.count("create", "Thing"), 1);
    assert_eq!(directory.fixture.count("create", "Sink"), 1);
    assert_eq!(
        directory.fixture.sink_values(),
        [serde_json::json!("bad-name")]
    );
    directory.assert_unlocked().await;
}

fn native_output_child(test: &str) -> std::process::Output {
    let args = ["--exact", test, "--ignored", "--nocapture"];
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(args)
        .output()
        .unwrap();
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        assert!(
            !String::from_utf8_lossy(bytes).contains(SECRET_PROBE),
            "secret in native command output: args={args:?}, stream={stream}"
        );
    }
    assert!(
        output.status.success(),
        "native command worker failed: {test}"
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "native command worker must execute exactly one test: {test}"
    );
    output
}

#[test]
fn secret_parameter_and_interpolation_command_output_is_private() {
    native_output_child("fn_declared_checks_tests::runtime_checks::native_secret_output_worker");
}

#[test]
#[ignore = "spawned by the output assertion with isolated stdout/stderr"]
fn native_secret_output_worker() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            secret_commands(false).await;
            secret_commands(true).await;
        });
}

#[test]
fn retained_json_command_prints_no_document_plan_out() {
    let output = native_output_child(
        "fn_declared_checks_tests::runtime_checks::native_refusal_output_worker_plan_out",
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let mut json_commands = 0;
    for following in stdout.split("BEGIN_JSON_COMMAND\n").skip(1) {
        let (body, _) = following.split_once("END_JSON_COMMAND").unwrap();
        assert_eq!(body, "", "refused JSON command must print no document");
        json_commands += 1;
    }
    assert_eq!(
        json_commands, 2,
        "root and module JSON output were observed"
    );
}

#[test]
#[ignore = "spawned by the output assertion with isolated stdout/stderr"]
fn native_refusal_output_worker_plan_out() {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            for name in [
                "c1-handle-late-valid",
                "c2-module-upstream-param-identity-late-valid",
            ] {
                let mut case = fixture_case(name);
                let source = case.child.as_mut().unwrap_or(&mut case.main);
                *source = source.replace(
                    "name = 'src'",
                    &format!("name = 'src' plain = secret('{SECRET_PROBE}')"),
                );
                let root = tempfile::Builder::new()
                    .prefix("plan project ' ")
                    .tempdir()
                    .unwrap();
                let directory = Directory::in_directory(&case, root);
                let saved = directory.path().join("saved.plan.json");
                for json in [false, true] {
                    if json {
                        println!("BEGIN_JSON_COMMAND");
                    }
                    let result = directory
                        .plan_output(if json { None } else { Some(&saved) }, json)
                        .await;
                    if json {
                        println!("END_JSON_COMMAND");
                    }
                    assert!(matches!(
                        &result,
                        Err(AppError::RetainedUserFunctionCall { .. })
                    ));
                    assert_eq!(
                        rendered(&result),
                        expected_retained_plan_refusal(directory.path())
                    );
                    assert!(!saved.exists());
                    assert_eq!(directory.fixture.count("create", "Sink"), 0);
                    directory.assert_unlocked().await;
                }
            }
        });
}
