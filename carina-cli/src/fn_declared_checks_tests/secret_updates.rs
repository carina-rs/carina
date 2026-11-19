use super::*;

#[tokio::test]
async fn secret_update_with_retained_call_uses_normal_plan_and_apply() {
    for (valid, refresh) in [(false, false), (true, false), (false, true), (true, true)] {
        let mut case = fixture_case("c1-handle-late-valid");
        case.main = "let sink = test.Sink { name = 'sink' value = secret('allowed') }\n".into();
        case.emitted = if valid { "allowed" } else { "bad-name" }.into();
        let dir = Directory::new(&case);
        dir.apply().await.unwrap();
        std::fs::write(dir.path().join("main.crn"), "let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\nlet sink = test.Sink { name = 'sink' value = secret(checked(src.emitted)) }\n").unwrap();
        let planned = if refresh {
            plan_refreshed(&dir).await
        } else {
            dir.plan(None).await
        };
        assert!(planned.is_ok(), "{}", rendered(&planned));
        assert!(
            planned.unwrap(),
            "source create and Sink update must be planned"
        );
        let applied = dir.apply().await;
        assert_eq!(applied.is_ok(), valid, "{}", rendered(&applied));
        assert!(!rendered(&applied).contains("Cannot save plan"));
        assert!(!rendered(&applied).contains("A saved plan cannot carry"));
        assert_eq!(dir.fixture.count("update", "Sink"), usize::from(valid));
        assert_eq!(dir.fixture.count("create", "Sink"), 1);
        assert_eq!(dir.fixture.count("create", "Thing"), 1);
        if !valid {
            assert_eq!(
                rendered(&applied),
                carina_core::resource::INSIDE_SECRET_MESSAGE
            );
        }
        dir.assert_unlocked().await;
    }
}

#[tokio::test]
async fn anonymous_secret_identity_can_hash_a_retained_call() {
    let mut case = fixture_case("c1-handle-late-valid");
    case.main = "let src = test.Thing { name = 'src' }\nfn checked(x: test.Name): String { x }\ntest.NamedThing { name = secret(checked(src.emitted)) }\n".into();
    let dir = Directory::new(&case);
    let plan = plan_refreshed(&dir).await;
    assert!(plan.is_ok(), "{}", rendered(&plan));
    let apply = dir.apply().await;
    assert!(apply.is_ok(), "{}", rendered(&apply));
    assert_eq!(dir.fixture.count("create", "NamedThing"), 1);
    dir.assert_unlocked().await;
}

async fn plan_refreshed(dir: &Directory) -> Result<bool, AppError> {
    commands::plan::run_plan_using(
        &dir.runtime,
        dir.path(),
        None,
        None,
        None,
        crate::DetailLevel::None,
        false,
        true,
        false,
        false,
        false,
        &dir.context(),
        |runtime, providers, _| {
            Ok(crate::wiring::ProviderLoad::for_test_with_provider_phase(
                runtime,
                dir.factories(),
                providers,
            ))
        },
    )
    .await
}
