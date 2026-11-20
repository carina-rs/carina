//! R7b: keep reference admission, and reject unbound prefix arguments in Live CLI validation.
use super::{Fixture, text};
use serde_json::{Value, json};

fn sources_fixture(sources: &Value) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for (name, source) in sources.as_object().unwrap() {
        let file = directory.path().join(name);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, source.as_str().unwrap()).unwrap();
        if name.ends_with(".crn") {
            files.push(name.clone());
        }
    }
    Fixture {
        directory,
        files,
        subject: "mock.test.resource.<pending>".into(),
    }
}

fn r6_sources() -> Value {
    serde_json::from_str(include_str!(
        "../fixtures/deferred_prefix/r7b/r6_inputs.json"
    ))
    .unwrap()
}

#[test]
fn r7b_m3_every_pending_prefix_form_keeps_main_reference_errors() {
    let inputs: Value =
        serde_json::from_str(include_str!("../fixtures/deferred_prefix/r7b/inputs.json")).unwrap();
    let main: Value = serde_json::from_str(include_str!(
        "../fixtures/deferred_prefix/r7b/main_reference_errors.json"
    ))
    .unwrap();
    assert_eq!(
        inputs.as_object().unwrap().len(),
        main.as_object().unwrap().len(),
        "every fixture needs captured main reference diagnostics"
    );
    let mut missing = Vec::new();
    for (case, sources) in inputs.as_object().unwrap() {
        let expected = &main[case];
        assert!(
            !expected["errors"].as_array().unwrap().is_empty(),
            "main must actually report a reference/resolution error for {case}"
        );
        let fixture = sources_fixture(sources);
        let human = fixture.cli(&["validate", "."]);
        let json_output = fixture.cli(&["validate", ".", "--json"]);
        assert_eq!(human.status.code(), Some(1), "{case}");
        assert_eq!(json_output.status.code(), Some(1), "{case}");
        let actual: Value = serde_json::from_slice(&json_output.stdout).unwrap();
        println!(
            "R7B_REFERENCE_CASE={case} {}",
            json!({"text":text(&human),"json":actual})
        );
        for error in expected["errors"].as_array().unwrap() {
            if !actual["errors"].as_array().unwrap().contains(error) {
                missing.push(format!(
                    "{case}: missing exact main JSON diagnostic {error}"
                ));
            }
        }
        for line in expected["text_lines"].as_array().unwrap() {
            if !text(&human)
                .lines()
                .any(|actual| actual == line.as_str().unwrap())
            {
                missing.push(format!("{case}: missing exact main text line {line}"));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn r7b_m3_direct_reference_has_replacement_and_main_reference_error() {
    let fixture = sources_fixture(&r6_sources()["m3_invalid_attribute"]);
    let message = "'name_prefix' must be known at plan time to generate 'name', but it depends on 'src.id', which is not resolved when names are generated";
    let reference =
        "mock.test.resource.<pending>: unknown attribute 'id' on 'src' in reference src.id";
    let human = fixture.cli(&["validate", "."]);
    assert_eq!(
        text(&human),
        format!(
            "Validating...\nError: mock.test.resource.<pending>: {message}\nError: mock.test.resource.<pending>.name_prefix: {reference}\n"
        )
    );
    let output = fixture.cli(&["validate", ".", "--json"]);
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        actual,
        json!({"status":"error","resource_count":0,"resources":[],"errors":[
            {"message":message,"severity":"error","code":null,"location":{"file":"main.crn","range":{"start":{"line":1,"column":22},"end":{"line":1,"column":42}}}},
            {"message":reference,"severity":"error","code":null,"location":{"file":"main.crn","range":{"start":{"line":1,"column":22},"end":{"line":1,"column":33}}}}
        ]})
    );
}

#[test]
fn r7b_m4_standalone_cli_reports_the_unbound_formal_argument() {
    let fixture = sources_fixture(&r6_sources()["m4_unimported_child"]);
    let message = "'name_prefix' must be known at plan time to generate 'name', but it depends on 'prefix', which is not resolved when names are generated";
    let human = fixture.cli(&["validate", "child"]);
    assert_eq!(human.status.code(), Some(1));
    assert_eq!(
        text(&human),
        format!("Validating...\nError: mock.test.resource.<pending>: {message}\n")
    );
    let output = fixture.cli(&["validate", "child", "--json"]);
    assert_eq!(output.status.code(), Some(1));
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        actual,
        json!({"status":"error","resource_count":0,"resources":[],"errors":[{
            "message":message,"severity":"error","code":null,
            "location":{"file":"child/main.crn","range":{"start":{"line":1,"column":22},"end":{"line":1,"column":42}}}
        }]})
    );
}

#[test]
fn r7b_m4_call_and_placement_errors_keep_their_existing_cli_output() {
    let sources = r6_sources();
    let expected: Value = serde_json::from_str(include_str!(
        "../fixtures/deferred_prefix/r7b/cli_controls.json"
    ))
    .unwrap();
    for (case, scopes) in expected.as_object().unwrap() {
        let fixture = sources_fixture(&sources[case]);
        for (scope, before) in scopes.as_object().unwrap() {
            let human = fixture.cli(&["validate", scope]);
            let output = fixture.cli(&["validate", scope, "--json"]);
            assert_eq!(text(&human), before["text"], "{case}");
            assert_eq!(human.status.code(), Some(1), "{case}");
            let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(actual, before["json"], "{case}");
        }
    }
}

#[tokio::test]
async fn r7b_r6_lsp_inventories_use_standalone_module_prefixes() {
    let sources = r6_sources();
    let expected: Value = serde_json::from_str(include_str!(
        "../fixtures/deferred_prefix/r7b/r6_standalone_lsp.json"
    ))
    .unwrap();
    for (case, before) in expected.as_object().unwrap() {
        assert_eq!(
            sources_fixture(&sources[case]).editor_inventory().await,
            *before,
            "standalone module prefixes and unchanged caller output: {case}"
        );
    }
}

#[tokio::test]
async fn r7b_m4_literal_caller_is_clean_module_reports_unbound_argument() {
    let fixture = Fixture::new(
        "let c = use { source = './child' }\nc { prefix = 'app' }\n",
        "",
        true,
    );
    let output = fixture.cli(&["validate", ".", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual["status"], "ok");
    assert_eq!(actual["resource_count"], 1);
    assert!(
        actual
            .get("errors")
            .is_none_or(|errors| errors.as_array().unwrap().is_empty())
    );
    for (file, diagnostics) in fixture.editor_inventory().await.as_object().unwrap() {
        let expected = if file == "child/main.crn" {
            json!([{
                "message": "'name_prefix' must be known at plan time to generate 'name', but it depends on 'prefix', which is not resolved when names are generated",
                "range": super::assignment(22), "severity": 1
            }])
        } else {
            json!([])
        };
        assert_eq!(*diagnostics, expected, "{file}");
    }
}

#[test]
fn r7b_m4_live_cli_preserves_argument_names_aliases_secrecy_and_joining() {
    let inputs: Value = serde_json::from_str(include_str!(
        "../fixtures/deferred_prefix/r7b/formal_inputs.json"
    ))
    .unwrap();
    for (case, dependency) in [
        ("bare", "'prefix'"),
        ("alias", "'prefix' (through 'p')"),
        ("interpolation", "'prefix'"),
        ("secret", "'(secret)'"),
        ("several", "'first' and 'second'"),
    ] {
        let sources = &inputs[case];
        let fixture = sources_fixture(sources);
        let message = format!(
            "'name_prefix' must be known at plan time to generate 'name', but it depends on {dependency}, which is not resolved when names are generated"
        );
        let human = fixture.cli(&["validate", "child"]);
        assert_eq!(human.status.code(), Some(1), "{case}");
        assert_eq!(
            text(&human),
            format!("Validating...\nError: mock.test.resource.<pending>: {message}\n"),
            "{case}"
        );
        let output = fixture.cli(&["validate", "child", "--json"]);
        let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
        let source = sources["child/main.crn"].as_str().unwrap().trim_end();
        assert_eq!(
            actual,
            json!({"status":"error","resource_count":0,"resources":[],"errors":[{
                "message":message,"severity":"error","code":null,
                "location":{"file":"child/main.crn","range":{"start":{"line":1,"column":source.find("name_prefix").unwrap()+1},"end":{"line":1,"column":source.rfind(" }").unwrap()+1}}}
            }]}),
            "{case}"
        );
    }
}
