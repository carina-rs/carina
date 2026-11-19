include!("support/conversion_review_fixture.rs");

impl Fixture {
    async fn observe(&self, label: &str, source: &str) -> Json {
        self.write("main.crn", source);
        let observation = json!({"label":label,"source":source,"validate":self.validate(),"lsp":self.lsp_file("main.crn",source),"plan":self.plan().await});
        let normalized = serde_json::to_string(&observation)
            .unwrap()
            .replace(self.path().to_str().unwrap(), "<fixture>");
        println!("R10 {normalized}");
        serde_json::from_str(&normalized).unwrap()
    }
}

#[path = "support/conversion_round13.rs"]
mod round13;

#[derive(Clone, Copy)]
enum DeferredCase {
    WrongTarget,
    WrongKind,
    LiteralKind,
    Ambiguous,
    Union,
    Valid,
    PlainMismatch,
    PlainLoop,
}
async fn deferred(case: DeferredCase, read: bool, resource_iterable: bool) {
    let fixture = Fixture::new();
    fixture.upstream(matches!(
        case,
        DeferredCase::WrongKind | DeferredCase::PlainMismatch | DeferredCase::PlainLoop
    ));
    let (query, sink, expression, rejection) = match case {
        DeferredCase::WrongTarget => (
            "Query",
            "Sink",
            "n as mock.Other",
            Some("conversion_destination"),
        ),
        DeferredCase::WrongKind => ("Query", "Sink", "n as mock.Name", None),
        DeferredCase::LiteralKind => (
            "Query",
            "Sink",
            "7 as mock.Name",
            Some("conversion_scalar_kind"),
        ),
        DeferredCase::Ambiguous => (
            "AmbiguousQuery",
            "AmbiguousSink",
            "n as mock.Name",
            Some("conversion_ambiguous_destination"),
        ),
        DeferredCase::Union => (
            "UnionQuery",
            "UnionSink",
            "p.output as mock.Name",
            Some("conversion_unsupported_position"),
        ),
        DeferredCase::Valid => ("Query", "Sink", "'ok' as mock.Name", None),
        DeferredCase::PlainMismatch => ("Query", "Sink", "7", None),
        DeferredCase::PlainLoop => ("Query", "Sink", "n", None),
    };
    let prelude = if resource_iterable {
        "let p = mock.Source {}\n"
    } else {
        "let up = upstream_state { source = './up' }\n"
    };
    let iterable = if resource_iterable {
        "p.items"
    } else {
        "up.names"
    };
    let body = if read {
        format!("read mock.{query} {{ attr = {expression} }}")
    } else {
        format!("mock.{sink} {{ attr = {expression} }}")
    };
    let producer = if matches!(case, DeferredCase::Union) {
        "let p = mock.Source {}\n"
    } else {
        ""
    };
    let source = format!("{prelude}{producer}let outputs = for n in {iterable} {{ {body} }}\n");
    let result = fixture
        .observe(
            &format!(
                "deferred read={read} resource_iterable={resource_iterable} {expression} {query}"
            ),
            &source,
        )
        .await;
    if read {
        assert_eq!(result["validate"]["status"], "error", "{result}");
        // The ordinary finalizer still blocks every deferred read. Target and
        // destination decisions precede source abstention; a known source kind
        // can also fail before finalization, including in a stopped run.
        let static_rejection = usize::from(matches!(
            case,
            DeferredCase::LiteralKind | DeferredCase::WrongTarget | DeferredCase::Ambiguous
        ));
        assert_eq!(
            conversion_count(&result["validate"]),
            static_rejection,
            "{result}"
        );
        assert_eq!(
            lsp_conversion_count(&result["lsp"]),
            static_rejection,
            "{result}"
        );
        if static_rejection == 1 {
            let code = rejection.expect("static rejection has its existing code");
            for findings in [&result["validate"]["errors"], &result["lsp"]] {
                assert!(
                    findings
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|finding| finding["code"] == code),
                    "{result}"
                );
            }
        }
        assert!(
            result["validate"]["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error["message"]
                    .as_str()
                    .unwrap()
                    .contains("only a managed resource body can be deferred until apply")),
            "{result}"
        );
        assert_eq!(result["plan"]["ok"], false, "{result}");
        assert!(
            result["plan"]["events"].as_array().unwrap().is_empty(),
            "{result}"
        );
    } else if let Some(code) = rejection {
        assert_eq!(conversion_count(&result["validate"]), 1, "{result}");
        assert!(
            result["validate"]["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error["code"] == code),
            "{result}"
        );
        assert_eq!(lsp_conversion_count(&result["lsp"]), 1, "{result}");
        assert_eq!(
            result["plan"]["ok"], false,
            "unjudged conversion reached planning: {result}"
        );
    } else if matches!(case, DeferredCase::Valid) {
        assert_eq!(result["validate"]["status"], "ok", "{result}");
        assert_eq!(conversion_count(&result["validate"]), 0);
        assert_eq!(lsp_conversion_count(&result["lsp"]), 0);
        assert_eq!(result["plan"]["ok"], true, "{result}");
    } else if matches!(case, DeferredCase::PlainLoop | DeferredCase::WrongKind) {
        assert_eq!(result["validate"]["status"], "ok", "{result}");
        assert_eq!(conversion_count(&result["validate"]), 0);
        assert_eq!(lsp_conversion_count(&result["lsp"]), 0);
        assert_eq!(result["plan"]["ok"], false, "{result}");
        assert!(
            result["plan"]["error"]
                .as_str()
                .unwrap()
                .contains("plan failed with 1 error(s)"),
            "{result}"
        );
    } else {
        assert_eq!(result["validate"]["status"], "error", "{result}");
        assert_eq!(conversion_count(&result["validate"]), 0);
        // The current LSP has no static finding for this no-as control.
        assert!(
            !result["lsp"]
                .as_array()
                .unwrap()
                .iter()
                .any(|diagnostic| diagnostic["severity"] == 1),
            "{result}"
        );
        assert_eq!(result["plan"]["ok"], false, "{result}");
    }
}
macro_rules! deferred_case {
    ($name:ident, $case:ident, $read:expr, $resource:expr) => {
        #[tokio::test]
        async fn $name() {
            deferred(DeferredCase::$case, $read, $resource).await;
        }
    };
}
deferred_case!(
    r10_deferred_read_wrong_identity_is_blocked_by_finalization,
    WrongTarget,
    true,
    false
);
deferred_case!(
    r10_deferred_read_int_source_is_blocked_by_finalization,
    WrongKind,
    true,
    false
);
deferred_case!(
    r10_deferred_read_literal_kind_is_blocked_by_finalization,
    LiteralKind,
    true,
    false
);
deferred_case!(
    r10_managed_known_int_operand_control,
    LiteralKind,
    false,
    false
);
deferred_case!(
    r10_deferred_read_ambiguous_destination_is_blocked_by_finalization,
    Ambiguous,
    true,
    false
);
deferred_case!(
    r10_deferred_read_unknown_union_is_blocked_by_finalization,
    Union,
    true,
    false
);
deferred_case!(
    r10_deferred_read_valid_conversion_is_blocked_by_finalization,
    Valid,
    true,
    false
);
deferred_case!(
    r10_read_plain_static_mismatch_control,
    PlainMismatch,
    true,
    false
);
deferred_case!(
    r10_managed_wrong_identity_control,
    WrongTarget,
    false,
    false
);
deferred_case!(
    r10_managed_wrong_scalar_kind_control,
    WrongKind,
    false,
    false
);
deferred_case!(r10_managed_ambiguous_control, Ambiguous, false, false);
deferred_case!(r10_managed_unknown_union_control, Union, false, false);
deferred_case!(r10_managed_valid_control, Valid, false, false);
deferred_case!(
    r10_managed_plain_mismatch_control,
    PlainMismatch,
    false,
    false
);
deferred_case!(
    r10_read_plain_int_loop_source_control,
    PlainLoop,
    true,
    false
);
deferred_case!(
    r10_managed_plain_int_loop_source_control,
    PlainLoop,
    false,
    false
);
deferred_case!(
    r10_resource_iterable_read_wrong_identity_is_blocked_by_finalization,
    WrongTarget,
    true,
    true
);
deferred_case!(
    r10_resource_iterable_read_valid_is_blocked_by_finalization,
    Valid,
    true,
    true
);

macro_rules! nested_shape {
    ($name:ident, $converted:expr, $pending:expr) => {
        #[tokio::test]
        async fn $name() {
            let fixture = Fixture::new();
            fixture.upstream(false);
            let condition = if $pending { "up.flag" } else { "false" };
            let expression = format!("if {condition} {{ [['x' as mock.Name]] }} else {{ ['y' as mock.Name] }}");
            let expression = if $converted { expression } else { expression.replace(" as mock.Name", "") };
            let source = format!("let up = upstream_state {{ source = './up' }}\nmock.ListSink {{ attr = {expression} }}\n");
            let result = fixture.observe(stringify!($name), &source).await;
            if $pending {
                assert_eq!(result["validate"]["status"], "error", "{result}");
                assert_eq!(conversion_count(&result["validate"]), 0, "{result}");
                assert!(result["validate"]["errors"].as_array().unwrap().iter().any(|e|e["message"].as_str().unwrap().contains("statically known")), "{result}");
                assert_eq!(result["plan"]["ok"], false, "{result}");
            } else {
                assert_eq!(result["validate"]["status"], "ok", "{result}");
                assert_eq!(conversion_count(&result["validate"]), 0, "{result}");
                assert_eq!(result["plan"]["ok"], true, "{result}");
            }
        }
    };
}
nested_shape!(
    r10_discarded_nested_shape_conversion_matches_plain_control,
    true,
    false
);
nested_shape!(r10_discarded_nested_shape_plain_control, false, false);
nested_shape!(
    r10_runtime_if_syntax_blocks_the_pending_shape_example,
    true,
    true
);
nested_shape!(r10_pending_nested_shape_plain_control, false, true);

#[tokio::test]
async fn r10_prefix_missing_value_reports_p3_on_cli_and_lsp() {
    let fixture = Fixture::new();
    let source = "mock.PrefixSink { attr_prefix = 'ok' as mock.PrefixName }\n";
    let result = fixture.observe("prefix-root", source).await;
    assert_eq!(conversion_count(&result["validate"]), 1, "{result}");
    assert!(
        result["validate"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["code"] == "conversion_unsupported_position"
                && e["message"] == "conversion cannot be checked here"),
        "{result}"
    );
    let plain = fixture
        .observe(
            "prefix-root-plain-control",
            &source.replace(" as mock.PrefixName", ""),
        )
        .await;
    let messages = |diagnostics: &Json| {
        diagnostics
            .as_array()
            .unwrap()
            .iter()
            .map(|diagnostic| {
                (
                    diagnostic["message"].clone(),
                    diagnostic["severity"].clone(),
                    diagnostic["code"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    // #4429 consumes the companion key while retaining its authored slot:
    // the plain prefix is accepted, and the converted prefix keeps CLI's P3.
    let mut expected = messages(&plain["lsp"]);
    expected.push((
        json!("conversion cannot be checked here"),
        json!(1),
        json!("conversion_unsupported_position"),
    ));
    assert_eq!(messages(&result["lsp"]), expected);
    assert_eq!(lsp_conversion_count(&result["lsp"]), 1, "{result}");
    assert_eq!(result["plan"]["ok"], false, "{result}");
}

async fn reinstanced_prefix(module: bool) {
    let fixture = Fixture::new();
    let body = "mock.PrefixSink { attr_prefix = 'ok' as mock.PrefixName }";
    let source = if module {
        fixture.write(
            "child/main.crn",
            &format!("arguments {{ unused: String = 'ok' }}\n{body}\n"),
        );
        "let child = use { source = './child' }\nlet first = child {}\nlet second = child {}\n"
            .to_owned()
    } else {
        format!("let sinks = for n in ['a','b'] {{ {body} }}\n")
    };
    let result = fixture
        .observe(&format!("reinstanced-prefix module={module}"), &source)
        .await;
    let module_lsp = if module {
        let child = std::fs::read_to_string(fixture.path().join("child/main.crn")).unwrap();
        Some(fixture.lsp_file("child/main.crn", &child))
    } else {
        None
    };
    println!(
        "R10-MODULE-LSP {}",
        serde_json::to_string(&module_lsp)
            .unwrap()
            .replace(fixture.path().to_str().unwrap(), "<fixture>")
    );
    assert_eq!(conversion_count(&result["validate"]), 1, "{result}");
    if !module {
        let plain = fixture
            .observe(
                "prefix-loop-plain-control",
                &source.replace(" as mock.PrefixName", ""),
            )
            .await;
        let messages = |diagnostics: &Json| {
            diagnostics
                .as_array()
                .unwrap()
                .iter()
                .map(|diagnostic| {
                    (
                        diagnostic["message"].clone(),
                        diagnostic["severity"].clone(),
                        diagnostic["code"].clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        // Preserve the ordinary loop warning and the retained companion's P3;
        // #4429 removes the plain twin's old unknown-attribute false positive.
        let mut expected = messages(&plain["lsp"]);
        expected.push((
            json!("conversion cannot be checked here"),
            json!(1),
            json!("conversion_unsupported_position"),
        ));
        assert_eq!(messages(&result["lsp"]), expected);
        assert_eq!(lsp_conversion_count(&result["lsp"]), 1, "{result}");
    }
}
#[tokio::test]
async fn r10_loop_reinstanced_missing_prefix_is_not_lost() {
    reinstanced_prefix(false).await;
}
#[tokio::test]
async fn r10_module_reinstanced_missing_prefix_is_not_lost() {
    reinstanced_prefix(true).await;
}

#[tokio::test]
async fn r10_review_read_binding_spelling_is_an_ordinary_syntax_error() {
    let fixture = Fixture::new();
    fixture.upstream(false);
    let source = "let up = upstream_state { source = './up' }\nfor n in up.names { let q = read mock.Query { attr = n as mock.Other } }\n";
    let result = fixture.observe("literal-review-spelling", source).await;
    assert_eq!(result["validate"]["status"], "error");
    assert_eq!(conversion_count(&result["validate"]), 0);
    assert!(
        result["validate"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["message"].as_str().unwrap().contains("Syntax error"))
    );
}

#[tokio::test]
async fn r10_schema_absence_is_owned_by_ordinary_diagnostics() {
    for loaded in [true, false] {
        let fixture = Fixture::new();
        let mut observations = Vec::new();
        for converted in [false, true] {
            let source = if converted {
                "mock.NoSchema { attr = 'ok' as mock.Other }\n"
            } else {
                "mock.NoSchema { attr = 'ok' }\n"
            };
            fixture.write("main.crn", source);
            let factories = if loaded { fixture.factories() } else { vec![] };
            let cli = carina_cli::commands::validate::validate_diagnostics_with_factories(
                &ResolvedPath::new(fixture.path()).unwrap(),
                carina_cli::wiring::LoadedProviderFactory::for_test(factories),
                Default::default(),
            )
            .json;
            let snapshot = loaded_provider_snapshot_from_factories(
                Arc::new(if loaded { fixture.factories() } else { vec![] }),
                SchemaRegistry::new(),
                carina_core::EnvironmentSnapshot::empty(),
            );
            let engine = carina_lsp::test_support::Diagnostics::new(
                fixture.path(),
                Arc::new(snapshot),
                if loaded { vec!["mock".into()] } else { vec![] },
            );
            let path = fixture.path().join("main.crn");
            let doc = carina_lsp::document::Document::new_with_path(
                source.into(),
                path.clone(),
                Arc::new(carina_core::parser::ProviderContext::new(
                    carina_core::EnvironmentSnapshot::empty(),
                )),
            );
            let lsp = engine.analyze(&doc, Some(&path));
            let runtime = carina_cli::test_support::TestCommandRuntime::new();
            carina_cli::commands::init::run_init(
                &runtime,
                fixture.path(),
                false,
                false,
                false,
                false,
            )
            .await
            .unwrap();
            let plan = carina_cli::test_support::run_plan_with_factories(
                &runtime,
                fixture.path(),
                carina_cli::wiring::LoadedProviderFactory::for_test(if loaded {
                    fixture.factories()
                } else {
                    vec![]
                }),
            )
            .await;
            let plan = match plan {
                Ok(changed) => json!({"ok":true,"changed":changed}),
                Err(error) => {
                    json!({"ok":false,"error":carina_cli::error::format_error_with_source_chain(&error)})
                }
            };
            let observation = json!({"label":"schema-absence","loaded":loaded,"source":source,"validate":cli,"lsp":lsp,"plan":plan});
            println!(
                "R10 {}",
                serde_json::to_string(&observation)
                    .unwrap()
                    .replace(fixture.path().to_str().unwrap(), "<fixture>")
            );
            assert_eq!(conversion_count(&observation["validate"]), 0);
            assert_eq!(lsp_conversion_count(&observation["lsp"]), 0);
            observations.push(observation);
        }
        assert_eq!(
            observations[0]["validate"]["status"],
            observations[1]["validate"]["status"]
        );
    }
}

fn binary(fixture: &Fixture, args: &[&str]) -> Json {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_carina"))
        .current_dir(fixture.path())
        .args(args)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .env(
            "CARINA_MOCK_STATE_FILE",
            fixture.path().join("provider-state.json"),
        )
        .env(
            "CARINA_MOCK_OP_LOG",
            fixture.path().join("provider-ops.log"),
        )
        .output()
        .unwrap();
    json!({"ok":output.status.success(),"code":output.status.code(),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)})
}
fn binary_seed(fixture: &Fixture, source: &str) {
    fixture.write("main.crn", source);
    let init = binary(fixture, &["init"]);
    assert_eq!(init["ok"], true, "{init}");
    let apply = binary(fixture, &["apply", "--auto-approve"]);
    assert_eq!(apply["ok"], true, "{apply}");
    fixture.write("provider-ops.log", "");
}

#[test]
fn r11_read_only_managed_calls_block_judge_conversion_failures() {
    let fixture = Fixture::new();
    binary_seed(
        &fixture,
        "let saved = mock.test.resource { name = 'persisted-name' }\n",
    );
    let source = "let saved = mock.test.resource { name = 'AUTHORED-UNJUDGED-R10' as Int }\n";
    fixture.write("main.crn", source);
    let validate = binary(&fixture, &["validate", "--json"]);
    let refresh = binary(&fixture, &["state", "refresh", "--lock=false"]);
    let refreshed_state =
        std::fs::read_to_string(fixture.path().join("carina.state.json")).unwrap();
    let refresh_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    fixture.write("provider-ops.log", "");
    let destroy = binary(&fixture, &["destroy", "--auto-approve", "--lock=false"]);
    let destroy_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    let observation = json!({"label":"read-only-managed","source":source,"validate":validate,"refresh":refresh,"refresh_ops":refresh_ops,"refreshed_state":serde_json::from_str::<Json>(&refreshed_state).unwrap(),"destroy":destroy,"destroy_ops":destroy_ops});
    println!(
        "R10 {}",
        serde_json::to_string(&observation)
            .unwrap()
            .replace(fixture.path().to_str().unwrap(), "<fixture>")
    );
    assert_eq!(validate["ok"], false, "{observation}");
    assert_eq!(refresh["ok"], false, "{observation}");
    assert_eq!(destroy["ok"], false, "{observation}");
    assert!(refresh_ops.is_empty(), "{observation}");
    assert!(destroy_ops.is_empty(), "{observation}");
    fixture.write("main.crn", &source.replace(" as Int", ""));
    let plain_refresh = binary(&fixture, &["state", "refresh", "--lock=false"]);
    assert_eq!(plain_refresh["ok"], true, "{plain_refresh}");
    let plain_destroy = binary(&fixture, &["destroy", "--auto-approve", "--lock=false"]);
    assert_eq!(plain_destroy["ok"], true, "{plain_destroy}");
    let plain_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    assert!(
        plain_ops.contains("read ") && plain_ops.contains("delete "),
        "{plain_ops}"
    );
    assert!(
        !refreshed_state.contains("AUTHORED-UNJUDGED-R10"),
        "{observation}"
    );
    assert!(refreshed_state.contains("persisted-name"), "{observation}");
}

#[test]
fn r10_state_refresh_cannot_dispatch_an_unjudged_data_source_argument() {
    let fixture = Fixture::new();
    binary_seed(
        &fixture,
        "let saved = mock.test.resource { name = 'persisted-name' }\nlet query = mock.iam.Roles { names = ['stored'] }\n",
    );
    // Keep the provider's query result, but remove its managed state row so
    // the only iam.Roles read below is the configured data-source call.
    let state_path = fixture.path().join("carina.state.json");
    let mut saved =
        carina_state::load_state(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
    assert!(
        saved
            .remove_resource("mock", "iam.Roles", "query")
            .is_some()
    );
    std::fs::write(&state_path, serde_json::to_vec(&saved).unwrap()).unwrap();

    let source = "let saved = mock.test.resource { name = 'persisted-name' }\nlet query = read mock.iam.Roles { name_regex = 'AUTHORED-UNJUDGED-R10' as Int }\n";
    fixture.write("main.crn", source);
    let validate = binary(&fixture, &["validate", "--json"]);
    let refresh = binary(&fixture, &["state", "refresh", "--lock=false"]);
    let operations = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    let state = std::fs::read_to_string(fixture.path().join("carina.state.json")).unwrap();
    fixture.write("provider-ops.log", "");
    let destroy = binary(&fixture, &["destroy", "--auto-approve", "--lock=false"]);
    let destroy_ops = std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap();
    let observation = json!({"label":"read-only-data-source","source":source,"validate":validate,"refresh":refresh,"operations":operations,"state":serde_json::from_str::<Json>(&state).unwrap(),"destroy":destroy,"destroy_ops":destroy_ops});
    println!(
        "R10 {}",
        serde_json::to_string(&observation)
            .unwrap()
            .replace(fixture.path().to_str().unwrap(), "<fixture>")
    );
    assert_eq!(validate["ok"], false, "{observation}");
    assert_eq!(refresh["ok"], false, "{observation}");
    assert_eq!(destroy["ok"], false, "{observation}");
    assert!(destroy_ops.is_empty(), "{observation}");
    assert!(
        !operations
            .lines()
            .any(|line| line.starts_with("read ") && line.contains("iam.Roles")),
        "unjudged authored data-source input crossed the provider boundary: {observation}"
    );
}

#[tokio::test]
async fn r10_prefix_plain_twin_matches_main_prefix_resolution() {
    let fixture = Fixture::new();
    let result = fixture
        .observe("prefix-plain", "mock.PrefixSink { attr_prefix = 'ok' }\n")
        .await;
    assert_eq!(result["validate"]["status"], "ok", "{result}");
    assert_eq!(result["plan"]["ok"], true, "{result}");
    assert_eq!(lsp_conversion_count(&result["lsp"]), 0);
    // #4429 closes the main-branch validate/LSP gap for plain companions.
    assert_eq!(result["lsp"], json!([]), "{result}");
}

#[tokio::test]
async fn r10_known_iterable_read_reaches_live_conversion_admission() {
    for (expression, code) in [
        ("n as mock.Name", None),
        ("n as mock.Other", Some("conversion_destination")),
        ("7 as mock.Name", Some("conversion_scalar_kind")),
    ] {
        let fixture = Fixture::new();
        let source = format!(
            "let queries = for n in ['ok'] {{ read mock.Query {{ attr = {expression} }} }}\n"
        );
        let result = fixture.observe("known-iterable-read", &source).await;
        assert_eq!(
            conversion_count(&result["validate"]),
            usize::from(code.is_some()),
            "{result}"
        );
        assert_eq!(
            lsp_conversion_count(&result["lsp"]),
            usize::from(code.is_some()),
            "{result}"
        );
        if let Some(code) = code {
            assert!(
                result["validate"]["errors"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|error| error["code"] == code),
                "{result}"
            );
            assert_eq!(result["plan"]["ok"], false, "{result}");
        } else {
            assert_eq!(result["validate"]["status"], "ok", "{result}");
            assert_eq!(result["plan"]["ok"], true, "{result}");
            assert!(
                result["plan"]["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|event| event["op"] == "read_data_source"),
                "{result}"
            );
        }
    }
}

#[tokio::test]
async fn r11_reinstanced_bare_operand_has_only_its_ordinary_slot_precheck() {
    let fixture = Fixture::new();
    let source = "let p = mock.Source {}\nlet sinks = for n in ['a', 'b'] { mock.Sink { attr = p as mock.Name } }\n";
    let measured = fixture
        .observe("r11-reinstanced-bare-operand", source)
        .await;
    assert_eq!(measured["validate"]["status"], "error", "{measured}");
    assert_eq!(conversion_count(&measured["validate"]), 0, "{measured}");
    assert_eq!(lsp_conversion_count(&measured["lsp"]), 0, "{measured}");
    assert!(
        measured["lsp"]
            .as_array()
            .unwrap()
            .iter()
            .any(|diagnostic| diagnostic["message"]
                .as_str()
                .unwrap()
                .contains("is a resource, not a value")),
        "{measured}"
    );
    let twin = fixture
        .observe(
            "r11-reinstanced-bare-plain",
            &source.replace(" as mock.Name", ""),
        )
        .await;
    let ordinary = |findings: &Json| {
        findings
            .as_array()
            .unwrap()
            .iter()
            .filter(|finding| {
                finding["message"]
                    .as_str()
                    .unwrap()
                    .contains("is a resource, not a value")
            })
            .map(|finding| {
                (
                    finding["message"].clone(),
                    finding["code"].clone(),
                    finding["severity"].clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ordinary(&measured["validate"]["errors"]),
        ordinary(&twin["validate"]["errors"])
    );
    assert_eq!(ordinary(&measured["lsp"]), ordinary(&twin["lsp"]));
}

#[tokio::test]
async fn r11_main_deferred_read_plain_whole_and_nested_references_are_blocked_by_finalization() {
    let fixture = Fixture::new();
    fixture.upstream(false);
    for body in [
        "read mock.Query { attr = n }",
        "read mock.NestedQuery { attr = { value = n } }",
    ] {
        let source = format!(
            "let up = upstream_state {{ source = './up' }}\nlet outputs = for n in up.names {{ {body} }}\n"
        );
        let observed = fixture
            .observe("main-deferred-read-plain-reference", &source)
            .await;
        assert_eq!(observed["validate"]["status"], "error", "{observed}");
        assert_eq!(conversion_count(&observed["validate"]), 0);
        assert_eq!(lsp_conversion_count(&observed["lsp"]), 0);
        for findings in [&observed["validate"]["errors"], &observed["lsp"]] {
            assert!(
                findings
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|finding| finding["message"]
                        .as_str()
                        .unwrap()
                        .contains("only a managed resource body can be deferred until apply")),
                "{observed}"
            );
        }
        assert_eq!(observed["plan"]["ok"], false, "{observed}");
        assert!(observed["plan"]["events"].as_array().unwrap().is_empty());
    }
}
