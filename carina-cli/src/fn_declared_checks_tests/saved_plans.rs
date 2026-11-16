use super::runtime_checks::{SECRET_PROBE, assert_no_secret};
use super::*;

#[tokio::test]
async fn refusal_has_project_apply_hint_plan_out() {
    for name in [
        "c1-handle-late-valid",
        "c2-module-upstream-param-identity-late-valid",
    ] {
        let root = tempfile::Builder::new()
            .prefix("plan project ' ")
            .tempdir()
            .unwrap();
        let directory = Directory::in_directory(&fixture_case(name), root);
        let saved = directory.path().join("saved.plan.json");

        let result = directory.plan(Some(&saved)).await;

        let diagnostic = rendered(&result);

        assert_no_secret(&diagnostic, "plan diagnostic");
        assert!(matches!(
            &result,
            Err(AppError::RetainedUserFunctionCall { .. })
        ));
        assert_eq!(diagnostic, expected_retained_plan_refusal(directory.path()));
        assert!(!saved.exists());
        assert_eq!(directory.fixture.count("create", "Sink"), 0);
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn retained_and_invalid_calls_refuse_json_with_exact_diagnostics() {
    for name in [
        "c1-handle-late-valid",
        "c2-module-upstream-param-identity-late-valid",
        "c2-module-param-const-literal-invalid",
    ] {
        let mut case = fixture_case(name);
        if !static_rejection(&case) {
            let source = case.child.as_mut().unwrap_or(&mut case.main);
            *source = source.replace(
                "name = 'src'",
                &format!("name = 'src' plain = secret('{SECRET_PROBE}')"),
            );
        }
        let directory = Directory::new(&case);

        let result = directory.plan_output(None, true).await;

        let diagnostic = rendered(&result);

        assert_no_secret(&diagnostic, "JSON diagnostic");
        assert!(result.is_err());

        assert!(!directory.path().join("saved.plan.json").exists());
        if static_rejection(&case) {
            assert_eq!(
                diagnostic,
                format!(
                    "{}/child/main.crn at line 3\nfunction 'g': parameter 'x': expected test.Name, got String",
                    directory.path().display()
                )
            );
        } else {
            assert!(matches!(
                &result,
                Err(AppError::RetainedUserFunctionCall { .. })
            ));
            assert_eq!(diagnostic, expected_retained_plan_refusal(directory.path()));
        }
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn problem_sentence_is_an_ordinary_savable_string() {
    let value = "the plan contains an unevaluated call to a user-defined function. A saved plan cannot carry this call.";
    let directory = Directory::new(&Case {
        name: "problem-looking-value".into(),
        main: format!(
            "test.Sink {{ name = 'sink' value = {} }}\n",
            serde_json::to_string(value).unwrap()
        ),
        child: None,
        emitted: "allowed".into(),
    });
    let saved = directory.path().join("saved.plan.json");
    directory.plan(Some(&saved)).await.unwrap();
    directory.apply_saved(&saved).await.unwrap();
    assert_eq!(directory.fixture.sink_values(), [serde_json::json!(value)]);
    directory.assert_unlocked().await;
}
