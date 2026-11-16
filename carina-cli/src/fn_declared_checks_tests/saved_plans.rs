use super::runtime_checks::{CapturedOutput, SECRET_PROBE, assert_no_secret};
use super::*;

struct WorkingDirectory(std::path::PathBuf);
impl WorkingDirectory {
    fn enter(path: &Path) -> Self {
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(path).unwrap();
        Self(previous)
    }
}
impl Drop for WorkingDirectory {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.0).unwrap();
    }
}

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
        let directory = Directory::in_directory(&measured_case(name), root);
        let saved = directory.path().join("saved.plan.json");
        let capture = CapturedOutput::start();
        let result = directory.plan(Some(&saved)).await;
        let (stdout, stderr) = capture.finish();
        let diagnostic = rendered(&result);
        assert_no_secret(&stdout, "plan stdout");
        assert_no_secret(&stderr, "plan stderr");
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
        let mut case = measured_case(name);
        if !static_rejection(&case) {
            let source = case.child.as_mut().unwrap_or(&mut case.main);
            *source = source.replace(
                "name = 'src'",
                &format!("name = 'src' plain = secret('{SECRET_PROBE}')"),
            );
        }
        let directory = Directory::new(&case);
        let _cwd = WorkingDirectory::enter(directory.path());
        let capture = CapturedOutput::start();
        let result = commands::plan::run_plan(
            &directory.runtime,
            Path::new("."),
            None,
            None,
            None,
            crate::DetailLevel::None,
            false,
            false,
            true,
            false,
            false,
            &directory.context(),
        )
        .await;
        let (stdout, stderr) = capture.finish();
        let diagnostic = rendered(&result);
        assert_no_secret(&stdout, "JSON stdout");
        assert_no_secret(&stderr, "JSON stderr");
        assert_no_secret(&diagnostic, "JSON diagnostic");
        assert!(result.is_err());
        assert!(stdout.is_empty());
        assert!(!directory.path().join("saved.plan.json").exists());
        if static_rejection(&case) {
            assert_eq!(
                diagnostic,
                "./child/main.crn at line 3\nfunction 'g': parameter 'x': expected test.Name, got String"
            );
        } else {
            assert!(matches!(
                &result,
                Err(AppError::RetainedUserFunctionCall { .. })
            ));
            assert_eq!(diagnostic, expected_retained_plan_refusal(Path::new(".")));
        }
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn problem_sentence_is_an_ordinary_savable_string() {
    let value = "a user-defined function call still needs its declared types checked against values available only during apply. A saved plan cannot carry this call.";
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
