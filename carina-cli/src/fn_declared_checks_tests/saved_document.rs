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
            || diagnostic != expected_retained_plan_refusal(directory.path())
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

#[test]
fn an_unrelated_serializer_using_the_same_sentence_gets_no_recovery_hint() {
    let error: serde_json::Error =
        serde::ser::Error::custom(carina_core::value::RetainedUserFunctionCallError);
    let actual = AppError::from_plan_serialization(error);
    assert_eq!(
        actual.to_string(),
        format!(
            "Failed to serialize plan: {}",
            carina_core::value::RetainedUserFunctionCallError
        )
    );
}
