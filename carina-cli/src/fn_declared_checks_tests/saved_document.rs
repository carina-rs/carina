use super::runtime_checks::assert_no_secret;
use super::*;

fn positions() -> Vec<(&'static str, Case)> {
    let base = "let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\n";
    let mut cases = Vec::new();
    for (position, tail) in [
        (
            "resource attribute",
            "test.Sink { name = 'sink' value = checked(src.emitted) }\n",
        ),
        (
            "data source",
            "let found = read test.Lookup { query = checked(src.emitted) }\n",
        ),
        (
            "export",
            "exports { output: String = checked(src.emitted) }\n",
        ),
        (
            "secret",
            "test.Sink { name = 'sink' value = secret(checked(src.emitted)) }\n",
        ),
    ] {
        cases.push((
            position,
            Case {
                name: position.into(),
                main: format!("{base}{tail}"),
                child: None,
                emitted: "allowed".into(),
            },
        ));
    }
    for (position, output_type, value) in [
        ("module output", "String", "checked(src.emitted)"),
        ("nested list", "list(String)", "[checked(src.emitted)]"),
        (
            "nested map",
            "map(String)",
            "{ key = checked(src.emitted) }",
        ),
    ] {
        cases.push((
            position,
            Case {
                name: position.into(),
                main: "let child = use { source = './child' }\nlet instance = child {}\n".into(),
                child: Some(format!(
                    "{base}attributes {{ output: {output_type} = {value} }}\n"
                )),
                emitted: "allowed".into(),
            },
        ));
    }
    for (position, child) in [
        (
            "module argument",
            "arguments { input: String }\nattributes { output: String = input }\n",
        ),
        (
            "require operand",
            "arguments { input: String }\nrequire input != 'rejected', 'required'\n",
        ),
    ] {
        cases.push((position,Case {name:position.into(),main:format!("{base}let child = use {{ source = './child' }}\nlet instance = child {{ input = checked(src.emitted) }}\n"),child:Some(child.into()),emitted:"allowed".into()}));
    }
    cases
}
async fn assert_refusal(json: bool) {
    let mut failures = Vec::new();
    for (position, case) in positions() {
        let directory = Directory::new(&case);
        let saved = directory.path().join("saved.plan.json");

        let result = directory
            .plan_output(if json { None } else { Some(&saved) }, json)
            .await;

        let diagnostic = rendered(&result);
        assert_no_secret(&diagnostic, "saved document diagnostic");
        assert!(
            matches!(&result, Err(AppError::RetainedUserFunctionCall { .. })),
            "{position}: {diagnostic}"
        );
        if result.is_ok()
            || diagnostic != expected_retained_plan_refusal(directory.path(), "checked")
            || saved.exists()
        {
            failures.push(format!(
                "{position}: exit={} file={} diagnostic={diagnostic}",
                i32::from(result.is_err()),
                saved.exists()
            ));
        }
        directory.assert_unlocked().await;
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
#[tokio::test]
async fn retained_calls_in_saved_document_positions_plan_out() {
    assert_refusal(false).await;
}
#[tokio::test]
async fn retained_calls_in_saved_document_positions_json() {
    assert_refusal(true).await;
}

#[tokio::test]
async fn admissible_saved_document_bytes() {
    let directory = Directory::new(&Case {
        name: "admissible document bytes".into(),
        main: "let child = use { source = './child' }\nlet instance = child { input = 'allowed' }\ntest.Sink { name = 'sink' value = instance.output }\n".into(),
        child: Some("arguments { input: String }\nlet src = test.Thing { name = input }\nfn identity(value: String): String { value }\nattributes { output: String = upper(identity(src.emitted)) }\n".into()),
        emitted: "allowed".into(),
    });
    let path = directory.path().join("saved.plan.json");
    directory.plan(Some(&path)).await.unwrap();
    let bytes = std::fs::read_to_string(path).unwrap();
    let document: serde_json::Value = serde_json::from_str(&bytes).unwrap();
    let normalized = bytes
        .replace(directory.path().to_str().unwrap(), "<project>")
        .replace(document["timestamp"].as_str().unwrap(), "<timestamp>");
    insta::assert_snapshot!("admissible_saved_document_bytes", normalized);
}

#[tokio::test]
async fn orphan_saved_state_preserves_the_persisted_secret_hash_plan_out() {
    let directory = Directory::new(&Case {
        name: "orphan secret snapshot".into(),
        main: "let src = test.Thing { name = 'src' plain = secret('private-control') }\n".into(),
        child: None,
        emitted: "allowed".into(),
    });
    directory.apply().await.unwrap();
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("state.json")).unwrap())
            .unwrap();
    let hash = state["resources"][0]["current"]["attributes"]["plain"].clone();
    assert!(hash["$secret"].is_string());
    std::fs::write(directory.path().join("main.crn"), "").unwrap();
    let path = directory.path().join("orphan.plan.json");
    directory.plan(Some(&path)).await.unwrap();
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(
        document["current_states"][0]["current"]["state"]["attributes"]["plain"]["RedactedSecret"]
            ["hash"],
        hash["$secret"]
    );
    directory.assert_unlocked().await;
}

async fn assert_evaluated_positions_save(json: bool) {
    for (position, case) in positions() {
        let directory = Directory::new(&case);
        let nested_source = case
            .child
            .as_ref()
            .is_some_and(|child| child.contains("let src = test.Thing"));
        let source_path = directory.path().join(if nested_source {
            "child/main.crn"
        } else {
            "main.crn"
        });
        let seed = if nested_source {
            "attributes { output: String = 'seed' }\nlet src = test.Thing { name = 'src' }\n"
        } else {
            "let src = test.Thing { name = 'src' }\n"
        };
        std::fs::write(&source_path, seed).unwrap();
        let applied = directory.apply().await;
        assert!(applied.is_ok(), "{position} seed: {}", rendered(&applied));
        std::fs::write(directory.path().join("main.crn"), &case.main).unwrap();
        if let Some(child) = &case.child {
            std::fs::write(directory.path().join("child/main.crn"), child).unwrap();
        }
        let saved = directory.path().join("saved.plan.json");
        let result = directory
            .plan_output(if json { None } else { Some(&saved) }, json)
            .await;
        assert!(result.is_ok(), "{position}: {}", rendered(&result));
        if !json {
            assert!(saved.is_file(), "{position}: no saved document");
            let document = std::fs::read_to_string(&saved).unwrap();
            assert!(!document.contains("ResolvedFunctionCall"), "{position}");
        }
        directory.assert_unlocked().await;
    }
}
#[tokio::test]
async fn evaluated_calls_in_all_saved_document_positions_plan_out() {
    assert_evaluated_positions_save(false).await;
}
#[tokio::test]
async fn evaluated_calls_in_all_saved_document_positions_json() {
    assert_evaluated_positions_save(true).await;
}

#[tokio::test]
async fn evaluated_secret_result_saved_apply_plan_out() {
    let directory = Directory::new(&Case {
        name: "evaluated secret result replay".into(),
        main: "let src = test.Thing { name = 'src' }\n".into(),
        child: None,
        emitted: "allowed".into(),
    });
    directory.apply().await.unwrap();
    std::fs::write(directory.path().join("main.crn"), "let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { secret(\"guard-${x}\") }\nlet sink = test.Sink { name = 'sink' value = checked(src.emitted) }\n").unwrap();
    let saved = directory.path().join("saved.plan.json");
    directory.plan(Some(&saved)).await.unwrap();
    assert!(
        !std::fs::read_to_string(&saved)
            .unwrap()
            .contains("guard-allowed")
    );
    directory.apply_saved(&saved).await.unwrap();
    let state = std::fs::read_to_string(directory.path().join("state.json")).unwrap();
    assert!(!state.contains("guard-allowed"));
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    let rows = state["resources"].as_array().unwrap();
    let sink = rows
        .iter()
        .find(|row| row["resource_type"] == "Sink")
        .unwrap();
    assert!(sink["current"]["attributes"]["value"]["$secret"].is_string());
    assert_eq!(directory.fixture.count("create", "Sink"), 1);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn saved_apply_ignores_a_changed_nonsecret_function_when_verifying_other_secrets_plan_out() {
    let directory = Directory::new(&Case {
        name: "nonsecret source remains pinned".into(),
        main: "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n"
            .into(),
        child: Some("arguments { label: String }\nlet src = test.Thing { name = 'src' }\n".into()),
        emitted: "allowed".into(),
    });
    directory.apply().await.unwrap();
    let body = "arguments { label: String }\nlet src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\nlet sink = test.Sink { name = 'sink' value = checked(src.emitted) }\nlet private = test.Sink { name = 'privacy' value = secret('private-control') }\n";
    std::fs::write(directory.path().join("child/main.crn"), body).unwrap();
    let saved = directory.path().join("saved.plan.json");
    directory.plan(Some(&saved)).await.unwrap();
    std::fs::write(
        directory.path().join("child/main.crn"),
        body.replace(
            "fn checked(x: test.Name): String { x }",
            "fn checked(x: test.Name): test.Name { \"prefix-${x}\" }",
        ),
    )
    .unwrap();
    let applied = directory.apply_saved(&saved).await;
    assert!(applied.is_ok(), "{}", rendered(&applied));
    assert_eq!(directory.fixture.count("create", "Sink"), 2);
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("state.json")).unwrap())
            .unwrap();
    let sink = state["resources"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["current"]["attributes"]["name"] == "sink")
        .unwrap();
    assert_eq!(sink["current"]["attributes"]["value"], "allowed");
    directory.assert_unlocked().await;
}
