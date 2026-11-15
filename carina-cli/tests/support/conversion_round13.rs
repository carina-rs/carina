//! Measurements share round 10's native provider and production command paths.
use super::*;

struct Scene {
    row: &'static str,
    target: &'static str,
    source: String,
    upstream: Option<(&'static str, &'static str, Json)>,
    module: Option<String>,
}

fn scenes() -> Vec<Scene> {
    let mut scenes = Vec::new();
    for (row, annotation, values) in [
        ("1 annotated list(String)", ": list(String)", json!(["ok"])),
        ("2 annotated list(Int)", ": list(Int)", json!([7])),
        ("3a unannotated string list", "", json!(["ok"])),
        ("3b unannotated int list", "", json!([7])),
    ] {
        scenes.push(Scene {
            row,
            target: "mock.Name",
            source: "let up = upstream_state { source = './up' }\nfor n in up.names { mock.Sink { attr = n AS } }\n".into(),
            upstream: Some(("names", annotation, values)),
            module: None,
        });
    }
    for (row, annotation, value) in [
        ("4a annotated String", ": String", json!("ok")),
        ("4b annotated Int", ": Int", json!(7)),
        ("4c unannotated string", "", json!("ok")),
        ("4d unannotated int", "", json!(7)),
    ] {
        scenes.push(Scene {
            row,
            target: "mock.Name",
            source:
                "let up = upstream_state { source = './up' }\nmock.Sink { attr = up.name AS }\n"
                    .into(),
            upstream: Some(("name", annotation, value)),
            module: None,
        });
    }
    for (row, source) in [
        (
            "5a unloaded provider",
            "provider unloaded {}\nlet p = unloaded.Source {}\nmock.Sink { attr = p.output AS }\n",
        ),
        (
            "5b undeclared source attribute",
            "let p = mock.Source {}\nmock.Sink { attr = p.undeclared AS }\n",
        ),
    ] {
        scenes.push(Scene {
            row,
            target: "mock.Name",
            source: source.into(),
            upstream: None,
            module: None,
        });
    }
    for (row, annotation, value) in [
        ("6a module output String", ": String", "p.output"),
        ("6b module output Int", ": Int", "p.number"),
        ("6c module output untyped", "", "p.output"),
    ] {
        scenes.push(Scene {
            row,
            target: "mock.Name",
            source: "let child = use { source = './child' }\nlet net = child {}\nmock.Sink { attr = net.out AS }\n".into(),
            upstream: None,
            module: Some(format!("let p = mock.Source {{ output = 'ok' number = 7 }}\nattributes {{ out{annotation} = {value} }}\n")),
        });
    }
    for (row, source) in [
        (
            "7 empty for",
            "for item in [] { mock.Sink { attr = item AS } }\n",
        ),
        (
            "8a resource handle",
            "let p = mock.Source {}\nmock.Sink { attr = p AS }\n",
        ),
        ("8b map", "mock.Sink { attr = { a = 'ok' } AS }\n"),
        ("8c list", "mock.Sink { attr = ['ok'] AS }\n"),
        (
            "8d struct",
            "fn entry(): struct { a: String } { { a = 'ok' } }\nmock.Sink { attr = entry() AS }\n",
        ),
        ("8e Bool", "mock.Sink { attr = true AS }\n"),
        ("8f Duration", "mock.Sink { attr = 5s AS }\n"),
        ("9a Int literal", "mock.Sink { attr = 7 AS }\n"),
        ("9b Int binding", "let n = 7\nmock.Sink { attr = n AS }\n"),
    ] {
        scenes.push(Scene {
            row,
            target: "mock.Name",
            source: source.into(),
            upstream: None,
            module: None,
        });
    }
    for (row, sink, target, value, deferred) in [
        (
            "10a static Int to Float",
            "FloatSink",
            "mock.Ratio",
            json!(42),
            false,
        ),
        (
            "10b deferred Int to Float",
            "FloatSink",
            "mock.Ratio",
            json!(7),
            true,
        ),
        (
            "11a static Int in Float range",
            "BoundedFloatSink",
            "mock.BoundedRatio",
            json!(7),
            false,
        ),
        (
            "11b static Int outside Float range",
            "BoundedFloatSink",
            "mock.BoundedRatio",
            json!(42),
            false,
        ),
        (
            "11c deferred Int in Float range",
            "BoundedFloatSink",
            "mock.BoundedRatio",
            json!(7),
            true,
        ),
        (
            "11d deferred Int outside Float range",
            "BoundedFloatSink",
            "mock.BoundedRatio",
            json!(42),
            true,
        ),
        (
            "12a static Float to Int",
            "IntSink",
            "mock.Count",
            json!(3.5),
            false,
        ),
        (
            "12b deferred Float to Int",
            "IntSink",
            "mock.Count",
            json!(3.5),
            true,
        ),
    ] {
        scenes.push(Scene {
            row,
            target,
            source: if deferred {
                format!("let up = upstream_state {{ source = './up' }}\nfor n in up.names {{ mock.{sink} {{ attr = n AS }} }}\n")
            } else {
                format!("mock.{sink} {{ attr = {value} AS }}\n")
            },
            upstream: deferred.then(|| ("names", if value.is_f64() { ": list(Float)" } else { ": list(Int)" }, json!([value]))),
            module: None,
        });
    }
    scenes
}

fn fixture(scene: &Scene, converted: bool) -> (Fixture, String) {
    let fixture = Fixture::new();
    if let Some((name, annotation, value)) = &scene.upstream {
        fixture.upstream(false);
        let literal = value.to_string();
        fixture.write(
            "up/main.crn",
            &format!("exports {{ {name}{annotation} = {literal} }}\n"),
        );
        let mut state = carina_state::StateFile::new();
        state.insert_export_json_for_test(*name, value.clone());
        std::fs::write(
            fixture.path().join("up/carina.state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
    }
    if let Some(module) = &scene.module {
        fixture.write("child/main.crn", module);
    }
    let source = scene.source.replace(
        "AS",
        &if converted {
            format!("as {}", scene.target)
        } else {
            String::new()
        },
    );
    (fixture, source)
}

#[tokio::test]
async fn r13_measure_scalar_sources_and_plain_twins() {
    for scene in scenes() {
        for converted in [false, true] {
            let (fixture, source) = fixture(&scene, converted);
            let cell = json!({"row": scene.row, "form": if converted { "as" } else { "plain" }});
            eprintln!("R13-START {cell}");
            let observation = fixture.observe(scene.row, &source).await;
            eprintln!("R13-END {cell}");
            println!(
                "R13 {}",
                json!({"row":scene.row,"target":scene.target,"form":if converted {"as"} else {"plain"},"module":scene.module,"upstream":scene.upstream,"observation":observation})
            );
        }
    }
}

#[tokio::test]
async fn r13_unknown_scalar_sources_defer_to_resolved_slot_admission() {
    for scene in scenes().into_iter().filter(|scene| {
        matches!(
            scene.row,
            "1 annotated list(String)"
                | "2 annotated list(Int)"
                | "3a unannotated string list"
                | "3b unannotated int list"
                | "4c unannotated string"
                | "4d unannotated int"
                | "7 empty for"
        )
    }) {
        let (fixture, source) = fixture(&scene, true);
        let result = fixture.observe(scene.row, &source).await;
        assert_eq!(result["validate"]["status"], "ok", "{result}");
        assert_eq!(lsp_conversion_count(&result["lsp"]), 0, "{result}");
        let wrong_value = matches!(
            scene.row,
            "2 annotated list(Int)" | "3b unannotated int list" | "4d unannotated int"
        );
        assert_eq!(result["plan"]["ok"], !wrong_value, "{result}");
        if wrong_value {
            assert!(
                result["plan"]["error"]
                    .as_str()
                    .unwrap()
                    .contains("plan failed with 1 error(s)"),
                "{result}"
            );
        } else if scene.row != "7 empty for" {
            assert_eq!(result["plan"]["changed"], true, "{result}");
        }
        assert!(
            result["plan"]["events"].as_array().unwrap().is_empty(),
            "{result}"
        );
    }
}

#[tokio::test]
async fn r13_known_nonscalars_have_a_distinct_typed_error() {
    for scene in scenes().into_iter().filter(|scene| {
        matches!(
            scene.row,
            "8b map"
                | "8c list"
                | "8d struct"
                | "8e Bool"
                | "8f Duration"
                | "9a Int literal"
                | "9b Int binding"
        )
    }) {
        let (fixture, source) = fixture(&scene, true);
        let result = fixture.observe(scene.row, &source).await;
        let code = if scene.row.starts_with('8') {
            "conversion_not_scalar"
        } else {
            "conversion_scalar_kind"
        };
        assert_eq!(conversion_count(&result["validate"]), 1, "{result}");
        assert_eq!(result["validate"]["errors"][0]["code"], code, "{result}");
        assert_eq!(lsp_conversion_count(&result["lsp"]), 1, "{result}");
        assert_eq!(result["lsp"][0]["code"], code, "{result}");
        assert_eq!(result["plan"]["ok"], false, "{result}");
    }
}

#[tokio::test]
async fn r13_unknown_static_source_with_a_known_value_is_checked_at_validate() {
    for (value, code) in [("'ok'", None), ("7", Some("conversion_destination"))] {
        let fixture = Fixture::new();
        let source =
            format!("fn supply() {{ {value} }}\nmock.Sink {{ attr = supply() as mock.Name }}\n");
        let result = fixture
            .observe("known value, unknown static source", &source)
            .await;
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
            assert_eq!(result["validate"]["errors"][0]["code"], code, "{result}");
        } else {
            assert_eq!(result["validate"]["status"], "ok", "{result}");
        }
        assert_eq!(result["plan"]["ok"], code.is_none(), "{result}");
    }
}

#[tokio::test]
async fn r13_annotated_string_loop_plans_and_creates_the_original_value() {
    let scene = scenes().remove(0);
    let (fixture, source) = fixture(&scene, true);
    fixture.write("main.crn", &source);
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    carina_cli::commands::init::run_init(&runtime, fixture.path(), false, false, false, false)
        .await
        .unwrap();
    assert_eq!(fixture.plan().await["ok"], true);
    carina_cli::commands::apply::run_apply_with_factories_for_test(
        &runtime,
        fixture.path(),
        carina_cli::wiring::LoadedProviderFactory::for_test(fixture.factories()),
    )
    .await
    .unwrap();
    let events = fixture.events.lock().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["op"], "create");
    assert_eq!(
        events[0].pointer("/attributes/attr/String"),
        Some(&json!("ok")),
        "{events:?}"
    );
}

#[tokio::test]
async fn r13_missing_scalar_judgement_keeps_an_ordinary_operand_failure() {
    let scene = scenes()
        .into_iter()
        .find(|scene| scene.row == "5b undeclared source attribute")
        .unwrap();
    for converted in [false, true] {
        let (fixture, source) = fixture(&scene, converted);
        let result = fixture.observe("ordinary operand failure", &source).await;
        assert_eq!(result["validate"]["status"], "error", "{result}");
        assert_eq!(conversion_count(&result["validate"]), 0, "{result}");
        assert_eq!(
            result["validate"]["errors"].as_array().unwrap().len(),
            1,
            "{result}"
        );
        let message = result["validate"]["errors"][0]["message"]
            .as_str()
            .unwrap()
            .to_lowercase();
        assert!(
            message.contains("unknown attribute") && message.contains("undeclared"),
            "{result}"
        );
        assert_eq!(lsp_conversion_count(&result["lsp"]), 0, "{result}");
        assert_eq!(result["plan"]["ok"], false, "{result}");
    }
}

#[tokio::test]
async fn r13b_upstream_source_type_owner_stage_matrix() {
    let mut cells = 0;
    for scene in scenes()
        .into_iter()
        .filter(|scene| scene.target == "mock.Name" && scene.upstream.is_some())
    {
        let (plain_fixture, plain_source) = fixture(&scene, false);
        let twin = plain_fixture.observe(scene.row, &plain_source).await;
        let (as_fixture, as_source) = fixture(&scene, true);
        let checked = as_fixture.observe(scene.row, &as_source).await;
        let static_wrong_kind = scene.row == "4b annotated Int";
        let wrong_value = matches!(
            scene.row,
            "2 annotated list(Int)"
                | "3b unannotated int list"
                | "4b annotated Int"
                | "4d unannotated int"
        );
        assert_eq!(
            conversion_count(&checked["validate"]),
            usize::from(static_wrong_kind),
            "{}: {checked}",
            scene.row
        );
        assert_eq!(
            lsp_conversion_count(&checked["lsp"]),
            usize::from(static_wrong_kind),
            "{}: {checked}",
            scene.row
        );
        if static_wrong_kind {
            assert_eq!(
                checked["validate"]["errors"][0]["code"], "conversion_scalar_kind",
                "{checked}"
            );
            assert_eq!(
                checked["lsp"][0]["code"], "conversion_scalar_kind",
                "{checked}"
            );
        } else {
            assert_eq!(checked["validate"]["status"], "ok", "{checked}");
        }
        assert_eq!(checked["plan"]["ok"], !wrong_value, "{checked}");
        if wrong_value {
            if twin["validate"]["status"] == "error" {
                assert_eq!(
                    checked["validate"]["status"], "error",
                    "twin={twin}; as={checked}"
                );
            }
            if twin["lsp"]
                .as_array()
                .unwrap()
                .iter()
                .any(|finding| finding["severity"] == 1)
            {
                assert!(
                    checked["lsp"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|finding| finding["severity"] == 1),
                    "twin={twin}; as={checked}"
                );
            }
            if twin["plan"]["ok"] == false {
                assert_eq!(checked["plan"]["ok"], false, "twin={twin}; as={checked}");
            }
        }
        cells += 6;
    }
    assert_eq!(cells, 48, "8 upstream rows x 2 forms x 3 surfaces/stages");
}

#[tokio::test]
async fn r13b_undeclared_operand_has_exactly_the_twins_ordinary_findings() {
    let scene = scenes()
        .into_iter()
        .find(|scene| scene.row == "5b undeclared source attribute")
        .unwrap();
    let (plain_fixture, plain_source) = fixture(&scene, false);
    let twin = plain_fixture.observe(scene.row, &plain_source).await;
    let (as_fixture, as_source) = fixture(&scene, true);
    let checked = as_fixture.observe(scene.row, &as_source).await;
    assert_eq!(
        checked["validate"]["errors"], twin["validate"]["errors"],
        "twin={twin}; as={checked}"
    );
    assert_eq!(checked["lsp"], twin["lsp"], "twin={twin}; as={checked}");
    assert_eq!(
        checked["plan"]["error"], twin["plan"]["error"],
        "twin={twin}; as={checked}"
    );
}

#[tokio::test]
async fn r13c_numeric_conversions_and_twins_share_kind_fit_and_bounds() {
    for scene in scenes()
        .into_iter()
        .filter(|scene| scene.target != "mock.Name")
    {
        let rejected = matches!(
            scene.row,
            "11b static Int outside Float range"
                | "11d deferred Int outside Float range"
                | "12a static Float to Int"
                | "12b deferred Float to Int"
        );
        let deferred = scene.upstream.is_some();
        for converted in [false, true] {
            let (fixture, source) = fixture(&scene, converted);
            let result = fixture.observe(scene.row, &source).await;
            let static_rejection = rejected && !deferred;
            assert_eq!(
                result["validate"]["status"],
                if static_rejection { "error" } else { "ok" },
                "{result}"
            );
            assert_eq!(
                conversion_count(&result["validate"]),
                usize::from(converted && static_rejection),
                "{result}"
            );
            assert_eq!(
                lsp_conversion_count(&result["lsp"]),
                usize::from(converted && static_rejection),
                "{result}"
            );
            if !static_rejection {
                assert!(result["lsp"].as_array().unwrap().is_empty(), "{result}");
            }
            if converted && static_rejection {
                let code = if scene.row.starts_with("12") {
                    "conversion_scalar_kind"
                } else {
                    "conversion_destination"
                };
                assert_eq!(result["validate"]["errors"][0]["code"], code, "{result}");
                assert_eq!(result["lsp"][0]["code"], code, "{result}");
            }
            if scene.row.starts_with("11") && static_rejection {
                assert!(
                    result["validate"]["errors"]
                        .to_string()
                        .contains("outside allowed range"),
                    "{result}"
                );
            }
            assert_eq!(result["plan"]["ok"], !rejected, "{result}");
            if rejected && deferred {
                assert!(
                    result["plan"]["error"]
                        .as_str()
                        .unwrap()
                        .contains("plan failed with 1 error(s)"),
                    "{result}"
                );
            }
            assert!(
                result["plan"]["events"].as_array().unwrap().is_empty(),
                "{result}"
            );
        }
    }
}

#[tokio::test]
async fn r13_deferred_int_to_float_uses_slot_admission_and_dispatches_the_original_value() {
    let fixture = Fixture::new();
    fixture.upstream(true);
    let source = "let up = upstream_state { source = './up' }\nfor n in up.names { mock.FloatSink { attr = n as mock.Ratio } }\n";
    let result = fixture.observe("deferred Int to Float", source).await;
    let runtime = carina_cli::test_support::TestCommandRuntime::new();
    let applied = carina_cli::commands::apply::run_apply_with_factories_for_test(
        &runtime,
        fixture.path(),
        carina_cli::wiring::LoadedProviderFactory::for_test(fixture.factories()),
    )
    .await;
    println!(
        "R13-FLOAT {}",
        json!({"observation":result,"apply":format!("{applied:?}"),"dispatch":fixture.events.lock().unwrap().clone()})
    );
    assert_eq!(result["validate"]["status"], "ok", "{result}");
    assert_eq!(conversion_count(&result["validate"]), 0, "{result}");
    assert_eq!(lsp_conversion_count(&result["lsp"]), 0, "{result}");
    assert_eq!(
        result["plan"]["ok"], true,
        "Int -> Float uses the same widening as the slot: {result}"
    );
    applied.unwrap();
    let events = fixture.events.lock().unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["op"], "create");
    assert_eq!(
        events[0].pointer("/attributes/attr/Int"),
        Some(&json!(7)),
        "{events:?}"
    );
}
