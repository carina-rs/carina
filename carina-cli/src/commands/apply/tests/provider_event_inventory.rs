use super::*;
#[path = "../../../../../carina-provider-mock/tests/support/provider_text_secret.rs"]
mod fixture;
use fixture::{EchoFactory, Failure, SECRET};

const CHILD: &str = "CARINA_SAVED_PLAN_EVENT_INVENTORY_CHILD";

fn capture(name: &str, deferred: bool) {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(exercise(deferred));
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .env("NO_COLOR", "1")
        .env_remove("CLICOLOR_FORCE")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(output.status.success(), "{stdout}{stderr}");
    let changed: String = SECRET.chars().rev().collect();
    for output in [&stdout, &stderr] {
        assert!(!output.contains(SECRET), "{output}");
        assert!(!output.contains(&changed), "{output}");
    }
    let expected = if deferred {
        vec![
            "warn [mock] mock.test.resource.children.one password: (secret)",
            "warn [mock] mock.test.resource.children.one strings: [(secret)]",
        ]
    } else {
        vec!["warn [mock] mock.test.resource.probe tags.Private: (secret)"]
    };
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("warn [mock]"))
            .collect::<Vec<_>>(),
        expected
    );
}

async fn exercise(deferred: bool) {
    let fixture = ApplyCancellationFixture::new().with_raw_config("");
    let directory = fixture.config_path();
    let source_text = if deferred {
        format!(
            "let producer = mock.test.resource {{ name = 'producer' password = 'public' tags = {{ one = 'public' }} }}\nlet children = for key, _ in producer.tags {{ mock.test.resource {{ name = key password = secret('{SECRET}') strings = secret(['{SECRET}']) }} }}\n"
        )
    } else {
        "let probe = mock.test.resource { name = 'probe' password = 'public' }\n".to_owned()
    };
    std::fs::write(directory.join("main.crn"), source_text).unwrap();
    let provider_text = if deferred {
        "provider mock {}\n".to_owned()
    } else {
        format!("provider mock {{ default_tags = {{ Private = secret('{SECRET}') }} }}\n")
    };
    // Use an explicit source so the saved artifact expects the injected factory,
    // rather than the built-in mock. No provider installation is involved.
    let artifact = directory.join("fixture.wasm");
    std::fs::write(&artifact, b"in-process test factory mock").unwrap();
    let provider_text = provider_text.replacen(
        "provider mock {",
        &format!(
            "provider mock {{ source = {}",
            serde_json::to_string(&format!("file://{}", artifact.display())).unwrap()
        ),
        1,
    );
    std::fs::write(directory.join("providers.crn"), provider_text).unwrap();
    std::fs::write(
        directory.join("backend.crn"),
        format!(
            "backend local {{ path = {} }}\n",
            serde_json::to_string(fixture.state_path()).unwrap()
        ),
    )
    .unwrap();
    let plan_path = directory.join("saved.plan.json");
    let failure = if deferred {
        Failure::CreateEvents
    } else {
        Failure::TagEvents
    };
    crate::test_support::run_saved_json_plan_with_factories(
        fixture.runtime(),
        directory,
        &plan_path,
        crate::wiring::LoadedProviderFactory::for_test(vec![Box::new(EchoFactory(failure))]),
    )
    .await
    .unwrap();
    let bytes = std::fs::read(&plan_path).unwrap();
    let saved = String::from_utf8_lossy(&bytes);
    assert!(!saved.contains(SECRET), "{saved}");
    assert!(
        !saved.contains(&SECRET.chars().rev().collect::<String>()),
        "{saved}"
    );
    // Decode the on-disk plan and take the same secret-verification transition
    // as run_apply_from_plan_with_observer_factory before its locked entry.
    let inspected = InspectedPlanFile::inspect(PlanFile::decode_json(&bytes).unwrap());
    let source = ResolvedPath::new(directory).unwrap();
    let loading_configs = inspected
        .provider_configs()
        .as_provider_configs()
        .iter()
        .map(carina_core::parser::ProviderConfig::loading_projection)
        .collect::<Vec<_>>();
    let providers = ProviderLoad::for_test_with_provider_phase(
        fixture.runtime(),
        crate::wiring::LoadedProviderFactory::for_test(vec![Box::new(EchoFactory(failure))]),
        &loading_configs,
    )
    .into_complete()
    .unwrap();
    let context = ProviderContext::new(carina_core::EnvironmentSnapshot::empty());
    let current =
        load_configuration_for_saved_rehydration(&source, &context, &SchemaRegistry::new())
            .unwrap();
    let bound = match inspected {
        InspectedPlanFile::Verified(plan) => ProviderBoundPlanFile::verified(plan, providers),
        InspectedPlanFile::NeedsSecretVerification(plan) => {
            let providers = providers
                .check_saved_plan_config_pair(
                    current.parsed,
                    current.unresolved_parsed,
                    &source,
                    false,
                    &current.duplicate_declarations,
                )
                .unwrap();
            ProviderBoundPlanFile::needs_secret_verification(plan, providers)
        }
    };
    let cancel = fixture.cancel_token();
    let backend = carina_state::PublishedBackend::for_test(fixture.backend_handle(), &cancel);
    let lock = StateLock::held(backend.acquire_lock("apply").await.unwrap());
    let outcome = run_apply_from_plan_locked_owned(
        fixture.runtime().color_mode(),
        bound,
        &context,
        true,
        backend.clone(),
        lock,
        ApplyTarget::SavedPlan {
            plan_file: &plan_path,
            source_dir: directory,
        },
        &source,
        cancel,
        &cli_observer_factory,
        NonZeroUsize::new(1).unwrap(),
        false,
    )
    .await;
    let error = finish_lock_command(
        fixture.runtime().color_mode(),
        FinalizationContext::new(&backend, directory),
        outcome,
    )
    .await
    .expect_err("mock emits events then fails");
    eprintln!(
        "Error: {}",
        crate::error::format_error_with_source_chain(&error)
    );
    if fixture.state_path().exists() {
        let saved = std::fs::read_to_string(fixture.state_path()).unwrap();
        assert!(!saved.contains(SECRET), "{saved}");
        assert!(
            !saved.contains(&SECRET.chars().rev().collect::<String>()),
            "{saved}"
        );
    }
}

#[test]
fn saved_plan_deferred_resource_events_mask_dispatched_secret_paths() {
    capture(
        "commands::apply::tests::provider_event_inventory::saved_plan_deferred_resource_events_mask_dispatched_secret_paths",
        true,
    );
}

#[test]
fn saved_plan_default_tag_events_mask_dispatched_secret_paths() {
    capture(
        "commands::apply::tests::provider_event_inventory::saved_plan_default_tag_events_mask_dispatched_secret_paths",
        false,
    );
}
