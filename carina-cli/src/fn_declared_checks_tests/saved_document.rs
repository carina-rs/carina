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
            "arguments { input: String require input != 'rejected', 'required' }\n",
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
        child: Some("arguments { input: String }\nlet src = test.Thing { name = input }\nfn identity(value) { value }\nattributes { output: String = upper(identity(src.emitted)) }\n".into()),
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
