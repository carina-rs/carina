use super::*;

#[tokio::test]
async fn sibling_function_files_and_pipe_calls_check_at_apply() {
    for module in [false, true] {
        for pipe in [false, true] {
            let value = if pipe {
                "src.emitted |> g()"
            } else {
                "g(src.emitted)"
            };
            let body = format!(
                "let src = test.Thing {{ name = 'src' }}\ntest.Sink {{ name = 'sink' value = {value} }}\n"
            );
            let directory = Directory::new(&Case {
                name: "sibling-function".into(),
                main: if module {
                    "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n".into()
                } else {
                    body.clone()
                },
                child: module.then(|| format!("arguments {{ label: String }}\n{body}")),
                emitted: "bad-name".into(),
            });
            std::fs::write(
                directory.path().join(if module {
                    "child/functions.crn"
                } else {
                    "functions.crn"
                }),
                "fn g(x: test.Name): String { x }\n",
            )
            .unwrap();
            directory.plan(None).await.unwrap();
            let result = directory.apply().await;
            let line = if module { 3 } else { 2 };
            assert_eq!(
                rendered(&result),
                format!("Invalid expression at line {line}: function 'g': parameter 'x': {SUFFIX}")
            );
            assert_eq!(directory.fixture.count("create", "Thing"), 1);
            assert_eq!(directory.fixture.count("create", "Sink"), 0);
            directory.assert_unlocked().await;
        }
    }
}

#[tokio::test]
async fn retained_data_source_argument_refuses_saved_documents_plan_out() {
    for json in [false, true] {
        let directory=Directory::new(&Case {name:"data-source-call".into(),main:"let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\nlet found = read test.Lookup { query = checked(src.emitted) }\ntest.Sink { name = 'sink' value = found.value }\n".into(),child:None,emitted:"allowed".into()});
        let saved = directory.path().join("saved.plan.json");
        let result = directory
            .plan_output(if json { None } else { Some(&saved) }, json)
            .await;
        assert!(
            matches!(&result, Err(AppError::RetainedUserFunctionCall { .. })),
            "{}",
            rendered(&result)
        );
        assert_eq!(
            rendered(&result),
            expected_retained_plan_refusal(directory.path())
        );
        assert!(!saved.exists());
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn state_known_valid_calls_refuse_saved_documents_plan_out() {
    for module in [false, true] {
        let body = "let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\ntest.Sink { name = 'sink' value = checked(src.emitted) }\n";
        let directory = Directory::new(&Case {
            name: "state-known-valid".into(),
            main: if module {
                "let child = use { source = './child' }\nlet instance = child { label = 'module' }\n".into()
            } else {
                body.into()
            },
            child: module.then(|| format!("arguments {{ label: String }}\n{body}")),
            emitted: "allowed".into(),
        });
        directory.apply().await.unwrap();
        assert!(!directory.plan(None).await.unwrap());
        let saved = directory.path().join("saved.plan.json");
        for json in [false, true] {
            let result = directory
                .plan_output(if json { None } else { Some(&saved) }, json)
                .await;
            assert!(matches!(
                &result,
                Err(AppError::RetainedUserFunctionCall { .. })
            ));
            assert_eq!(
                rendered(&result),
                expected_retained_plan_refusal(directory.path())
            );
            assert!(!saved.exists());
        }
        assert_eq!(directory.fixture.count("create", "Sink"), 1);
        directory.assert_unlocked().await;
    }
}

#[tokio::test]
async fn untyped_wrapper_of_pending_builtin_saved_plan_control() {
    let directory = Directory::new(&Case {
        name: "untyped-wrapper".into(),
        main: "let src = test.Thing { name = 'src' }\nfn outer(x) { x }\ntest.Sink { name = 'sink' value = outer(upper(src.emitted)) }\n".into(),
        child: None,
        emitted: "allowed".into(),
    });
    let saved = directory.path().join("saved.plan.json");
    directory.plan(Some(&saved)).await.unwrap();
    directory.apply_saved(&saved).await.unwrap();
    assert_eq!(
        directory.fixture.sink_values(),
        [serde_json::json!("ALLOWED")]
    );
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn unselected_typed_call_is_not_dispatched_saved_plan_control() {
    let directory = Directory::new(&Case {
        name: "unselected-call".into(),
        main: "let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\nif false { test.Sink { name = 'sink' value = checked(src.emitted) } }\n".into(),
        child: None,
        emitted: "bad-name".into(),
    });
    directory.plan(None).await.unwrap();
    let saved = directory.path().join("saved.plan.json");
    directory.plan(Some(&saved)).await.unwrap();
    directory.apply_saved(&saved).await.unwrap();
    assert_eq!(directory.fixture.count("create", "Thing"), 1);
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn retained_import_identifier_refuses_saved_documents_plan_out() {
    let directory = Directory::new(&Case {
        name: "import-identifier".into(),
        main: "let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\ntest.Sink { name = 'sink' value = 'allowed' }\nimport { to = test.Sink 'sink' id = \"${checked(src.emitted)}\" }\n".into(),
        child: None,
        emitted: "allowed".into(),
    });
    directory.plan(None).await.unwrap();
    let saved = directory.path().join("saved.plan.json");
    for json in [false, true] {
        let result = directory
            .plan_output(if json { None } else { Some(&saved) }, json)
            .await;
        assert!(matches!(
            &result,
            Err(AppError::RetainedUserFunctionCall { .. })
        ));
        assert_eq!(
            rendered(&result),
            expected_retained_plan_refusal(directory.path())
        );
        assert!(!saved.exists());
    }
    assert_eq!(directory.fixture.count("create", "Sink"), 0);
    directory.assert_unlocked().await;
}

#[tokio::test]
async fn returning_a_resource_handle_is_rejected_before_dispatch() {
    for (tail, expected) in [
        (
            "let alias = f(src)\ntest.Sink { name = 'sink' value = alias.emitted }",
            "provenance",
        ),
        (
            "fn consume(r: test.Thing): String { 'allowed' }\ntest.Sink { name = 'sink' value = consume(f(src)) }",
            "handle",
        ),
        (
            "test.Sink { name = 'sink' value = f(src).emitted }",
            "handle",
        ),
    ] {
        let directory = Directory::new(&Case {
            name: "handle-return".into(),
            main: format!(
                "let src = test.Thing {{ name = 'src' }}\nfn f(r: test.Thing): test.Thing {{ r }}\n{tail}\n"
            ),
            child: None,
            emitted: "allowed".into(),
        });
        let diagnostic = if expected == "provenance" {
            format!(
                "missing dependency edge to src provenance in {}/main.crn",
                directory.path().display()
            )
        } else {
            format!(
                "{}/main.crn:2:35: Invalid expression at line 2: 'r' is a resource handle, not a value; reference one of its attributes, e.g. 'r.<attribute>'",
                directory.path().display()
            )
        };
        let planned = directory.plan(None).await;
        assert_eq!(rendered(&planned), diagnostic);
        let applied = directory.apply().await;
        assert_eq!(rendered(&applied), diagnostic);
        assert_eq!(directory.fixture.count("create", "Thing"), 0);
        assert_eq!(directory.fixture.count("create", "Sink"), 0);
        directory.assert_unlocked().await;
    }
}
