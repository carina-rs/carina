// Reuse the round-10/13 fixture, including its event-recording native provider.
include!("support/conversion_review_fixture.rs");

fn emit(case: &str, converted: bool, observation: &Json) {
    println!(
        "R15 {}",
        json!({"case":case,"form":if converted {"as"} else {"plain"},"observation":observation})
    );
}

async fn measure(fixture: &Fixture, case: &str, converted: bool, source: &str) -> Json {
    fixture.write("main.crn", source);
    fixture.events.lock().unwrap().clear();
    let validate = fixture.validate();
    let validate_events = fixture.events.lock().unwrap().clone();
    fixture.events.lock().unwrap().clear();
    let lsp = fixture.lsp_file("main.crn", source);
    let lsp_events = fixture.events.lock().unwrap().clone();
    let plan = fixture.plan().await;
    let observation = json!({"source":source,"validate":validate,"validate_events":validate_events,"lsp":lsp,"lsp_events":lsp_events,"plan":plan});
    let observation: Json = serde_json::from_str(
        &observation
            .to_string()
            .replace(fixture.path().to_str().unwrap(), "<fixture>"),
    )
    .unwrap();
    emit(case, converted, &observation);
    observation
}

fn accepted(observation: &Json) -> bool {
    observation["validate"]["status"] == "ok"
        && !observation["lsp"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["severity"] == 1)
        && observation["plan"]["ok"] == true
}

async fn alias_uses(family: &str, selection: &str) {
    let (prefix, uses) = match family {
        "reference" => (
            "let p = mock.Source {}\nlet v = p.output AS\n",
            ["mock.Sink { attr = v }\n", "mock.ListSink { attr = [v] }\n"],
        ),
        "struct" => (
            "let v = 'ab' AS\nlet m = { name = v }\n",
            ["mock.StructSink { cfg = m }\n", "mock.Sink { attr = v }\n"],
        ),
        "known_union" => (
            "let v = 'ab' AS\n",
            [
                "let direct = mock.Sink { attr = v }\n",
                "let union = mock.Sink { choice = v }\n",
            ],
        ),
        "union" => (
            "let p = mock.Source {}\nlet v = p.output AS\n",
            [
                "let direct = mock.Sink { attr = v }\n",
                "let union = mock.Sink { choice = v }\n",
            ],
        ),
        other => panic!("unknown measurement family {other}"),
    };
    let body = match selection {
        "first_only" => uses[0].to_owned(),
        "second_only" => uses[1].to_owned(),
        "forward" => uses.concat(),
        "reverse" => [uses[1], uses[0]].concat(),
        other => panic!("unknown measurement selection {other}"),
    };
    let case = format!("1/{family}/{selection}");
    let mut observations = Vec::new();
    for converted in [false, true] {
        let source =
            format!("{prefix}{body}").replace(" AS", if converted { " as mock.Name" } else { "" });
        observations.push(measure(&Fixture::new(), &case, converted, &source).await);
    }
    // Each use and both orders are independent pins; collect both forms before asserting.
    if family == "union" && selection != "first_only" {
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
        let message =
            "conversion of an unknown value under a union destination is not supported yet";
        let error = &converted["validate"]["errors"][0];
        assert_eq!(error["code"], "conversion_unsupported_position");
        assert_eq!(error["message"], message);
        assert_eq!(
            error["location"]["range"]["start"],
            json!({"line":2,"column":9})
        );
        let diagnostics = converted["lsp"].as_array().unwrap();
        let conversion = diagnostics
            .iter()
            .find(|d| d["code"] == "conversion_unsupported_position")
            .unwrap();
        assert_eq!(conversion["message"], message);
        assert_eq!(
            conversion["range"]["start"],
            json!({"line":1,"character":8})
        );
        assert!(
            !converted.to_string().contains("conversion_source"),
            "{converted}"
        );
        assert_eq!(converted["plan"]["ok"], false);
    } else {
        assert!(
            accepted(&observations[1]),
            "a shared conversion must admit at every destination: {observations:#?}"
        );
    }
    assert!(
        accepted(&observations[0]),
        "plain control: {observations:#?}"
    );
}

macro_rules! alias_case {
    ($name:ident, $family:literal, $selection:literal) => {
        #[tokio::test]
        async fn $name() {
            alias_uses($family, $selection).await;
        }
    };
}
alias_case!(r15_reference_alias_direct_alone, "reference", "first_only");
alias_case!(r15_reference_alias_list_alone, "reference", "second_only");
alias_case!(r15_reference_alias_both_forward, "reference", "forward");
alias_case!(r15_reference_alias_both_reverse, "reference", "reverse");
alias_case!(r15_struct_alias_struct_alone, "struct", "first_only");
alias_case!(r15_struct_alias_direct_alone, "struct", "second_only");
alias_case!(r15_struct_alias_both_forward, "struct", "forward");
alias_case!(r15_struct_alias_both_reverse, "struct", "reverse");
alias_case!(r15_union_alias_direct_alone, "union", "first_only");
alias_case!(r15_union_alias_member_alone, "union", "second_only");
alias_case!(r15_union_alias_both_forward, "union", "forward");
alias_case!(r15_union_alias_both_reverse, "union", "reverse");

alias_case!(r16_known_union_alias_both_forward, "known_union", "forward");
alias_case!(r16_known_union_alias_both_reverse, "known_union", "reverse");

async fn mixed_operand(operator: &str, condition: &str) {
    let case = format!("2/{operator}/{condition}");
    let mut observations = Vec::new();
    for converted in [false, true] {
        let fixture = Fixture::new();
        fixture.upstream(false);
        let prefix = "let up = upstream_state { source = './up' }\n";
        let operand = match operator {
            "if" => format!("if {condition} {{ 'a' }} else {{ 1 }}"),
            "coalesce" => match condition {
                "string" => "'a' ?? 1".into(),
                "int" => "1 ?? 'a'".into(),
                "unknown" => "up.name ?? 1".into(),
                other => panic!("unknown coalesce case {other}"),
            },
            other => panic!("unknown operator {other}"),
        };
        if operator == "coalesce" && condition == "unknown" {
            fixture.write("up/main.crn", "exports { name: String = 'a' }\n");
            let mut state = carina_state::StateFile::new();
            state.insert_export_json_for_test("name", json!("a"));
            fixture.write(
                "up/carina.state.json",
                &serde_json::to_string(&state).unwrap(),
            );
        }
        let expression = if converted {
            format!("({operand}) as mock.Name")
        } else {
            operand
        };
        let source = format!("{prefix}mock.Sink {{ attr = {expression} }}\n");
        observations.push(measure(&fixture, &case, converted, &source).await);
    }
    let twin = &observations[0];
    let converted = &observations[1];
    assert_eq!(
        converted["validate"]["status"], twin["validate"]["status"],
        "as must preserve the twin's validate-stage outcome: {observations:#?}"
    );
    if !twin["lsp"].as_array().unwrap().is_empty() {
        assert!(
            !converted["lsp"].as_array().unwrap().is_empty(),
            "as must preserve the twin's LSP-stage diagnostic (including a warning): {observations:#?}"
        );
    }
    assert_eq!(
        converted["plan"]["ok"], twin["plan"]["ok"],
        "as must preserve the twin's plan-stage outcome: {observations:#?}"
    );
}
macro_rules! mixed_case {
    ($name:ident, $operator:literal, $condition:literal) => {
        #[tokio::test]
        async fn $name() {
            mixed_operand($operator, $condition).await;
        }
    };
}
mixed_case!(
    r15_mixed_if_true_keeps_the_twins_static_rejection,
    "if",
    "true"
);
mixed_case!(
    r15_mixed_if_false_keeps_the_twins_static_rejection,
    "if",
    "false"
);
mixed_case!(
    r15_mixed_if_unknown_keeps_the_twins_static_rejection,
    "if",
    "up.flag"
);
mixed_case!(
    r15_mixed_coalesce_string_keeps_the_twins_static_rejection,
    "coalesce",
    "string"
);
mixed_case!(
    r15_mixed_coalesce_int_keeps_the_twins_static_rejection,
    "coalesce",
    "int"
);
mixed_case!(
    r15_mixed_coalesce_unknown_keeps_the_twins_static_rejection,
    "coalesce",
    "unknown"
);

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
    json!({"ok":output.status.success(),"exit":output.status.code(),"stdout":String::from_utf8_lossy(&output.stdout),"stderr":String::from_utf8_lossy(&output.stderr)})
}

fn operations(fixture: &Fixture) -> String {
    std::fs::read_to_string(fixture.path().join("provider-ops.log")).unwrap_or_default()
}
fn saved_state(fixture: &Fixture) -> Json {
    serde_json::from_slice(&std::fs::read(fixture.path().join("carina.state.json")).unwrap())
        .unwrap()
}

fn native_read_only_gate(fixture: &Fixture) -> Json {
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
    match providers.validate_read_only_config(loaded.parsed, &path, &loaded.duplicate_declarations)
    {
        Ok(_) => json!({"ok":true}),
        Err(error) => {
            json!({"ok":false,"error":carina_cli::error::format_error_with_source_chain(&error),"debug":format!("{error:?}")})
        }
    }
}

async fn read_only_operand(target: &str) {
    let mut native = Vec::new();
    for converted in [false, true] {
        let fixture = Fixture::new();
        let expression = if converted {
            format!("p.name as {target}")
        } else {
            "p.name".into()
        };
        let source = format!(
            "let p = mock.test.resource {{ name = 'stored' }}\nlet q = read mock.Query {{ attr = {expression} }}\n"
        );
        let mut observation =
            measure(&fixture, &format!("3/native/{target}"), converted, &source).await;
        observation["read_only_gate"] = native_read_only_gate(&fixture);
        emit(&format!("3/native-gate/{target}"), converted, &observation);
        native.push(observation);
    }
    // State refresh has no injected-factory entry. Use the built-in mock's
    // actual query command. It has no named scalar custom types: mock.Other
    // is a loaded wrong identity in the native fixture above, but an unknown
    // target here. Keep that distinction explicit in the observations/report.
    let mut observations = Vec::new();
    for converted in [false, true] {
        let fixture = Fixture::new();
        fixture.write("main.crn", "let p = mock.test.resource { name = 'stored' }\nlet q = mock.iam.Roles { names = ['stored-result'] }\n");
        let init = binary(&fixture, &["init"]);
        assert_eq!(init["ok"], true, "{init}");
        let seed = binary(&fixture, &["apply", "--auto-approve", "--lock=false"]);
        assert_eq!(seed["ok"], true, "{seed}");
        let mut baseline = carina_state::check_and_migrate(
            &serde_json::to_string(&saved_state(&fixture)).unwrap(),
        )
        .unwrap()
        .into_state();
        assert!(baseline.remove_resource("mock", "iam.Roles", "q").is_some());
        fixture.write(
            "carina.state.json",
            &serde_json::to_string(&baseline).unwrap(),
        );
        let baseline = saved_state(&fixture);
        let provider_baseline = std::fs::read(fixture.path().join("provider-state.json")).unwrap();
        let expression = if converted {
            format!("p.name as {target}")
        } else {
            "p.name".into()
        };
        let source = format!(
            "let p = mock.test.resource {{ name = 'stored' }}\nlet q = read mock.iam.Roles {{ name_regex = {expression} }}\nexports {{ seen = q.names }}\n"
        );
        fixture.write("main.crn", &source);
        fixture.write("provider-ops.log", "");
        let validate_process = binary(&fixture, &["validate", "--json"]);
        let validate: Json =
            serde_json::from_str(validate_process["stdout"].as_str().unwrap()).unwrap();
        // The fixture's native factory includes the built-in query schema;
        // its extra named mock.Other type is intentionally NOT installed in
        // this snapshot, so LSP sees exactly the command's built-in schemas.
        let loaded = LoadedProviderSnapshot::for_test(
            {
                let mut schemas = SchemaRegistry::new();
                for schema in carina_provider_mock::builtin_schemas() {
                    schemas.insert("mock", schema);
                }
                schemas
            },
            Arc::new(Vec::new()),
        );
        let path = fixture.path().join("main.crn");
        let doc = carina_lsp::document::Document::new_with_path(
            source.clone(),
            path.clone(),
            Arc::new(carina_core::parser::ProviderContext::new(
                carina_core::EnvironmentSnapshot::empty(),
            )),
        );
        let lsp = carina_lsp::test_support::Diagnostics::new(
            fixture.path(),
            Arc::new(loaded),
            vec!["mock".into()],
        )
        .analyze(&doc, Some(&path));
        let plan = binary(&fixture, &["plan", "--refresh=false"]);
        let plan_events = operations(&fixture);
        fixture.write("provider-ops.log", "");
        let refresh = binary(&fixture, &["state", "refresh", "--lock=false"]);
        let refreshed = saved_state(&fixture);
        let refresh_events = operations(&fixture);
        fixture.write(
            "carina.state.json",
            &serde_json::to_string(&baseline).unwrap(),
        );
        std::fs::write(
            fixture.path().join("provider-state.json"),
            &provider_baseline,
        )
        .unwrap();
        fixture.write("provider-ops.log", "");
        let destroy = binary(&fixture, &["destroy", "--auto-approve", "--lock=false"]);
        let observation = json!({"source":source,"provider":"built-in mock; no named scalar mock.Other type","validate":validate,"validate_process":validate_process,"lsp":lsp,"plan":plan,"plan_events":plan_events,"refresh":refresh,"refresh_events":refresh_events,"refreshed_state":refreshed,"destroy":destroy,"destroy_events":operations(&fixture),"data_source_input_from_saved_p":{"name_regex":"stored"}});
        let observation: Json = serde_json::from_str(
            &observation
                .to_string()
                .replace(fixture.path().to_str().unwrap(), "<fixture>"),
        )
        .unwrap();
        emit(&format!("3/commands/{target}"), converted, &observation);
        observations.push(observation);
    }
    assert_eq!(
        native[1]["validate"]["status"], "error",
        "native control: {native:#?}"
    );
    assert_eq!(conversion_count(&native[1]["validate"]), 1, "{native:#?}");
    assert_eq!(lsp_conversion_count(&native[1]["lsp"]), 1, "{native:#?}");
    assert_eq!(
        observations[0]["refresh"]["ok"], true,
        "plain refresh control: {observations:#?}"
    );
    assert_eq!(
        observations[0]["destroy"]["ok"], true,
        "plain destroy control: {observations:#?}"
    );
    assert!(
        observations[0]["destroy_events"]
            .as_str()
            .unwrap()
            .lines()
            .any(|event| event == "delete test.resource.p"),
        "plain destroy must still delete the saved resource: {observations:#?}"
    );
    for command in ["refresh", "destroy"] {
        assert_eq!(
            observations[1][command]["ok"], false,
            "the rejected conversion must block {command}: {observations:#?}"
        );
    }
    let data_source_read = |events: &Json| {
        events
            .as_str()
            .unwrap()
            .lines()
            .any(|line| line.starts_with("read ") && line.contains("iam.Roles"))
    };
    let mut violations = Vec::new();
    if data_source_read(&observations[1]["refresh_events"]) {
        violations.push("state refresh dispatched the query despite its rejected conversion");
    }
    if data_source_read(&observations[1]["destroy_events"]) {
        violations.push("destroy dispatched the query despite its rejected conversion");
    }
    if native[1]["read_only_gate"]["ok"] != false {
        violations.push("the native read-only gate accepted the rejected target/identity");
    }
    assert!(
        violations.is_empty(),
        "{violations:?}\nnative={native:#?}\ncommands={observations:#?}"
    );
}

#[tokio::test]
async fn r15_read_only_unknown_target_cannot_dispatch_a_data_source_read() {
    read_only_operand("mock.DoesNotExist").await;
}
#[tokio::test]
async fn r15_read_only_wrong_identity_cannot_dispatch_a_data_source_read() {
    read_only_operand("mock.Other").await;
}

// Module arguments are the grammar's supported source of a deferred value if;
// a direct upstream Bool condition is rejected at parse time instead.
#[tokio::test]
async fn r15_mixed_if_deferred_module_argument_matches_the_twins_stage() {
    let mut observations = Vec::new();
    for converted in [false, true] {
        let fixture = Fixture::new();
        fixture.upstream(false);
        let operand = "if flag { 'a' } else { 1 }";
        let expression = if converted {
            format!("({operand}) as mock.Name")
        } else {
            operand.into()
        };
        let module = format!("arguments {{ flag: Bool }}\nmock.Sink {{ attr = {expression} }}\n");
        fixture.write("child/main.crn", &module);
        let source = "let up = upstream_state { source = './up' }\nlet child = use { source = './child' }\nlet net = child { flag = up.flag }\n";
        let mut observation =
            measure(&fixture, "2/if/deferred-module-argument", converted, source).await;
        observation["module"] = json!(module);
        emit("2/if/deferred-module-argument", converted, &observation);
        observations.push(observation);
    }
    let twin = &observations[0];
    let converted = &observations[1];
    assert_eq!(
        converted["validate"]["status"], twin["validate"]["status"],
        "converted module-if cannot lose the twin's static finding: {observations:#?}"
    );
    if !twin["lsp"].as_array().unwrap().is_empty() {
        assert!(
            !converted["lsp"].as_array().unwrap().is_empty(),
            "converted module-if cannot lose the twin's LSP finding: {observations:#?}"
        );
    }
    assert_eq!(
        converted["plan"]["ok"], twin["plan"]["ok"],
        "{observations:#?}"
    );
}

#[tokio::test]
async fn r16_resource_handle_conversion_keeps_both_ordinary_twin_findings() {
    let mut observations = Vec::new();
    for converted in [false, true] {
        let fixture = Fixture::new();
        let operand = if converted { "p as mock.Name" } else { "p" };
        let source = format!("let p = mock.Source {{}}\nmock.Sink {{ attr = {operand} }}\n");
        observations.push(measure(&fixture, "r16/resource-handle", converted, &source).await);
    }
    for surface in ["validate", "lsp"] {
        let messages = |o: &Json| {
            let entries = if surface == "validate" {
                &o[surface]["errors"]
            } else {
                &o[surface]
            };
            entries
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d["message"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        let twin = messages(&observations[0]);
        assert_eq!(twin.len(), 2, "{observations:#?}");
        assert_eq!(messages(&observations[1]), twin, "{observations:#?}");
    }
    assert_eq!(conversion_count(&observations[1]["validate"]), 0);
    assert_eq!(lsp_conversion_count(&observations[1]["lsp"]), 0);
}

#[test]
fn r16_sibling_value_conversion_is_reported_despite_an_ordinary_error() {
    let fixture = Fixture::new();
    let source =
        "let converted = 'too-long-for-name' as mock.Name\nmock.Sink { attr = missing.output }\n";
    fixture.write("main.crn", source);
    fixture.write(
        "sibling.crn",
        "let sibling = mock.Sink { attr = converted }\n",
    );
    let diagnostics = serde_json::to_value(fixture.lsp_file("main.crn", source)).unwrap();
    println!("R16-SIBLING {diagnostics}");
    assert!(
        diagnostics
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["message"].as_str().unwrap().contains("missing")),
        "{diagnostics}"
    );
    assert_eq!(lsp_conversion_count(&diagnostics), 1, "{diagnostics}");
    let error = diagnostics
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "conversion_destination")
        .unwrap();
    assert_eq!(error["range"]["start"], json!({"line":0,"character":16}));
}
