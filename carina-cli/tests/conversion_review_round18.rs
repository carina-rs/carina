include!("support/conversion_review_fixture.rs");

fn read_only_gate(fixture: &Fixture) -> Json {
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    let context =
        carina_core::parser::ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let path = ResolvedPath::new(fixture.path()).unwrap();
    let loaded = carina_core::config_loader::load_configuration_with_config(
        &path,
        &context,
        &SchemaRegistry::new(),
    )
    .unwrap();
    let providers = carina_cli::wiring::ProviderLoad::for_test_with_provider_phase(
        &runtime,
        carina_cli::wiring::LoadedProviderFactory::for_test(fixture.factories()),
        &loaded.parsed.providers,
    );
    fixture.events.lock().unwrap().clear();
    let result = match providers.validate_read_only_config(
        loaded.parsed,
        &path,
        &loaded.duplicate_declarations,
    ) {
        Ok(_) => json!({"ok":true}),
        Err(error) => {
            json!({"ok":false,"error":carina_cli::error::format_error_with_source_chain(&error),"debug":format!("{error:?}")})
        }
    };
    json!({"result":result,"events":fixture.events.lock().unwrap().clone()})
}

async fn observe(fixture: &Fixture, case: &str, source: &str, read_only: bool) -> Json {
    fixture.write("main.crn", source);
    fixture.events.lock().unwrap().clear();
    let validate = fixture.validate();
    let validate_events = fixture.events.lock().unwrap().clone();
    fixture.events.lock().unwrap().clear();
    let lsp = fixture.lsp_file("main.crn", source);
    let lsp_events = fixture.events.lock().unwrap().clone();
    let plan = fixture.plan().await;
    let gate = read_only.then(|| read_only_gate(fixture));
    let observation = json!({"case":case,"source":source,"validate":validate,"validate_events":validate_events,"lsp":lsp,"lsp_events":lsp_events,"plan":plan,"read_only_gate":gate});
    let normalized = observation
        .to_string()
        .replace(fixture.path().to_str().unwrap(), "<fixture>");
    println!("R18 {normalized}");
    serde_json::from_str(&normalized).unwrap()
}

#[tokio::test]
async fn r18_no_as_type_resolution_keeps_mains_cause_text() {
    let mut observations = Vec::new();
    for (case, source) in [
        (
            "function_parameter",
            "fn f(x: mock.Nope): String { x }\nmock.Sink { attr = f('a') }\n",
        ),
        (
            "annotated_let",
            "let x: mock.Nope = 'a'\nmock.Sink { attr = x }\n",
        ),
        (
            "module_argument",
            "let child = use { source = './child' }\nlet instance = child { value = 'a' }\n",
        ),
        (
            "function_return",
            "fn f(x: String): mock.Nope { x }\nmock.Sink { attr = f('a') }\n",
        ),
        (
            "function_call_access",
            "fn f(x: String): mock.Nope { x }\nmock.Sink { attr = f('a').name }\n",
        ),
    ] {
        let fixture = Fixture::new();
        if case == "module_argument" {
            fixture.write(
                "child/main.crn",
                "arguments { value: mock.Nope }\nmock.Sink { attr = value }\n",
            );
        }
        observations.push(observe(&fixture, case, source, false).await);
    }
    // Main's UnknownType adapter displays the typed cause verbatim. These
    // expected strings come from that Display and the existing sink context.
    for observation in observations {
        assert_eq!(conversion_count(&observation["validate"]), 0);
        assert_eq!(lsp_conversion_count(&observation["lsp"]), 0);
        let (validate, lsp, plan): (Vec<&str>, Vec<&str>, &str) = match observation["case"]
            .as_str()
            .unwrap()
        {
            "function_parameter" => (
                vec![
                    "function parameter 'f' parameter 'x': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "function 'f': return value: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                vec![
                    "function parameter 'f' parameter 'x': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "function 'f': return value: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                "<fixture>/main.crn at line 1\nfunction parameter 'f' parameter 'x': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'\n<fixture>/main.crn at line 1\nfunction 'f': return value: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
            ),
            "module_argument" => (
                vec![
                    "unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "module child argument 'value' in <fixture>/main.crn at line 2: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "<fixture>/child/main.crn: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name' at line 2",
                    "argument 'value': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                vec![
                    "module child argument 'value' in <fixture>/main.crn at line 2: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "<fixture>/child/main.crn: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name' at line 2",
                    "argument 'value': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                "<fixture>/child/main.crn at line 2\nunknown custom type 'mock.Nope'; suggestion: use 'mock.Name'\n<fixture>/main.crn at line 2\nmodule child argument 'value' in <fixture>/main.crn at line 2: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'\n<fixture>/child/main.crn at line 2\n<fixture>/child/main.crn: unknown custom type 'mock.Nope'; suggestion: use 'mock.Name' at line 2\n<fixture>/child/main.crn at line 2\nunknown custom type 'mock.Nope'; suggestion: use 'mock.Name'\n<fixture>/child/main.crn at line 1\nargument 'value': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
            ),
            "function_return" => (
                vec![
                    "function return type 'f': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                vec![
                    "function 'f': return value: expected mock.Nope, got string 'a': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "function return type 'f': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                "<fixture>/main.crn at line 1\nfunction return type 'f': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'\n<fixture>/main.crn at line 2\nunknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
            ),
            "function_call_access" => (
                vec![
                    "function return type 'f': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                vec![
                    "function 'f': return value: expected mock.Nope, got string 'a': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                    "function return type 'f': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
                ],
                "<fixture>/main.crn at line 1\nfunction return type 'f': unknown custom type 'mock.Nope'; suggestion: use 'mock.Name'",
            ),
            "annotated_let" => {
                // Neither parent accepts typed-let syntax; keep the existing
                // syntax-error class instead of adding grammar for this probe.
                assert_eq!(
                    observation["validate"]["errors"][0]["message"],
                    "Syntax error: expected discard_pattern"
                );
                assert!(
                    observation["plan"]["setup_error"]
                        .as_str()
                        .unwrap()
                        .contains("Syntax error:")
                );
                continue;
            }
            case => panic!("unclassified no-as measurement {case}"),
        };
        fn messages(items: &Json) -> Vec<&str> {
            items
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["message"].as_str().unwrap())
                .collect()
        }
        assert_eq!(
            messages(&observation["validate"]["errors"]),
            validate,
            "{observation:#?}"
        );
        assert_eq!(messages(&observation["lsp"]), lsp, "{observation:#?}");
        assert_eq!(observation["plan"]["error"], plan, "{observation:#?}");
    }
}

async fn nested(case: &str, target: &str) {
    let mut observations = Vec::new();
    for converted in [false, true] {
        let fixture = Fixture::new();
        let operand = if converted {
            format!("p.name as {target}")
        } else {
            "p.name".into()
        };
        let query = match case {
            "alias" => {
                format!("read mock.AliasedQuery {{ rule {{ condition {{ name = {operand} }} }} }}")
            }
            "canonical" => format!(
                "read mock.AliasedQuery {{ rules = [{{ conditions = [{{ name = {operand} }}] }}] }}"
            ),
            "union" => format!("read mock.StructUnionQuery {{ attr = {{ b = {operand} }} }}"),
            other => panic!("unknown measurement case {other}"),
        };
        let source = format!("let p = mock.test.resource {{ name = 'stored' }}\nlet q = {query}\n");
        observations.push(
            observe(
                &fixture,
                &format!("{case}/{target}/{}", if converted { "as" } else { "plain" }),
                &source,
                true,
            )
            .await,
        );
    }
    let converted = &observations[1];
    assert_eq!(
        conversion_count(&converted["validate"]),
        1,
        "{observations:#?}"
    );
    assert_eq!(
        lsp_conversion_count(&converted["lsp"]),
        1,
        "{observations:#?}"
    );
    assert_eq!(converted["plan"]["ok"], false, "{observations:#?}");
    // A successful public read-only validation gate would allow a command to
    // dispatch this query. These native schemas have no binary-command factory.
    assert_eq!(
        converted["read_only_gate"]["result"]["ok"], false,
        "the rejected conversion must block the read-only dispatch gate: {observations:#?}"
    );
}

macro_rules! nested_case {
    ($name:ident,$case:literal,$target:literal) => {
        #[tokio::test]
        async fn $name() {
            nested($case, $target).await;
        }
    };
}
nested_case!(
    r18_nested_alias_unknown_target_blocks_read_only_dispatch,
    "alias",
    "mock.DoesNotExist"
);
nested_case!(
    r18_nested_alias_wrong_identity_blocks_read_only_dispatch,
    "alias",
    "mock.Other"
);
nested_case!(
    r18_canonical_nested_unknown_target_blocks_read_only_dispatch,
    "canonical",
    "mock.DoesNotExist"
);
nested_case!(
    r18_canonical_nested_wrong_identity_blocks_read_only_dispatch,
    "canonical",
    "mock.Other"
);
nested_case!(
    r18_union_unknown_target_blocks_read_only_dispatch,
    "union",
    "mock.DoesNotExist"
);
nested_case!(
    r18_union_wrong_identity_blocks_read_only_dispatch,
    "union",
    "mock.Other"
);

#[tokio::test]
async fn r18_plain_nested_alias_wrong_kind_measures_the_twins_owner() {
    let mut observations = Vec::new();
    for (case, query) in [
        (
            "alias_int",
            "read mock.AliasedQuery { rule { condition { name = 7 } } }",
        ),
        (
            "canonical_int",
            "read mock.AliasedQuery { rules = [{ conditions = [{ name = 7 }] }] }",
        ),
        (
            "union_int",
            "read mock.StructUnionQuery { attr = { b = 7 } }",
        ),
    ] {
        let fixture = Fixture::new();
        observations.push(observe(&fixture, case, &format!("let q = {query}\n"), true).await);
    }
    for observation in observations {
        assert_eq!(observation["validate"]["status"], "error", "{observation}");
        assert_eq!(
            conversion_count(&observation["validate"]),
            0,
            "{observation}"
        );
        assert_eq!(
            lsp_conversion_count(&observation["lsp"]),
            0,
            "{observation}"
        );
    }
}

#[tokio::test]
async fn r18_no_as_upstream_annotation_control_keeps_ordinary_messages() {
    let fixture = Fixture::new();
    fixture.upstream(true);
    let observation = observe(&fixture, "upstream_plain_control", "let up = upstream_state { source = './up' }\nfor n in up.names { mock.Sink { attr = n } }\n", false).await;
    assert_eq!(conversion_count(&observation["validate"]), 0);
    assert_eq!(lsp_conversion_count(&observation["lsp"]), 0);
    assert!(!observation.to_string().contains("type resolution failed"));
}
