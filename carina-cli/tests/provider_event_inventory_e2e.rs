use carina_cli::test_support::TestCommandRuntime;
use carina_cli::wiring::LoadedProviderFactory;
use std::process::Command;

#[path = "../../carina-provider-mock/tests/support/provider_text_secret.rs"]
mod fixture;
use fixture::{EchoFactory, Failure, SECRET};

const CHILD: &str = "CARINA_PROVIDER_EVENT_INVENTORY_CHILD";

fn capture(name: &str, expected: &[&str], exercise: impl FnOnce()) {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        exercise();
        return;
    }
    let output = Command::new(std::env::current_exe().unwrap())
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
    assert_eq!(
        stderr
            .lines()
            .filter(|line| line.starts_with("warn [mock]"))
            .collect::<Vec<_>>(),
        expected
    );
}

fn apply_case(source: &str, provider: &str, failure: Failure) {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state.json");
    std::fs::write(dir.path().join("main.crn"), source).unwrap();
    std::fs::write(dir.path().join("providers.crn"), provider).unwrap();
    std::fs::write(
        dir.path().join("backend.crn"),
        format!(
            "backend local {{ path = {} }}\n",
            serde_json::to_string(&state).unwrap()
        ),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("carina-backend.lock"),
        serde_json::to_vec(
            &serde_json::json!({"backend_type":"local","attributes":{"path":state}}),
        )
        .unwrap(),
    )
    .unwrap();
    let runtime = TestCommandRuntime::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let iterations = if failure == Failure::ResumeTagEvents {
        2
    } else {
        1
    };
    for _iteration in 0..iterations {
        let result = rt.block_on(
            carina_cli::commands::apply::run_apply_with_factories_for_test(
                &runtime,
                dir.path(),
                LoadedProviderFactory::for_test(vec![Box::new(EchoFactory(failure))]),
            ),
        );
        let error = result.expect_err("fixture provider must fail after dispatch");
        eprintln!(
            "Error: {}",
            carina_cli::error::format_error_with_source_chain(&error)
        );
        if state.exists() {
            let saved = std::fs::read_to_string(&state).unwrap();
            assert!(!saved.contains(SECRET), "{saved}");
            assert!(
                !saved.contains(&SECRET.chars().rev().collect::<String>()),
                "{saved}"
            );
        }
    }
}

#[test]
fn deferred_resource_events_mask_dispatched_secret_paths() {
    capture(
        "deferred_resource_events_mask_dispatched_secret_paths",
        &[
            "warn [mock] mock.test.resource.children.one password: (secret)",
            "warn [mock] mock.test.resource.children.one strings: [(secret)]",
        ],
        || {
            apply_case(
                &format!(
                    "let producer = mock.test.resource {{ name = 'producer' password = 'public' tags = {{ one = 'public' }} }}\nlet children = for key, _ in producer.tags {{ mock.test.resource {{ name = key password = secret('{SECRET}') strings = secret(['{SECRET}']) }} }}\n"
                ),
                "provider mock {}\n",
                Failure::CreateEvents,
            )
        },
    );
}

#[test]
fn default_tag_events_mask_dispatched_secret_paths() {
    capture(
        "default_tag_events_mask_dispatched_secret_paths",
        &["warn [mock] mock.test.resource.probe tags.Private: (secret)"],
        || {
            apply_case(
                "let probe = mock.test.resource { name = 'probe' password = 'public' }\n",
                &format!("provider mock {{ default_tags = {{ Private = secret('{SECRET}') }} }}\n"),
                Failure::TagEvents,
            )
        },
    );
}

#[test]
fn resumed_create_events_mask_dispatched_secret_paths() {
    capture(
        "resumed_create_events_mask_dispatched_secret_paths",
        &["warn [mock] mock.test.resource.probe tags.Private: (secret)"],
        || {
            apply_case(
                "let probe = mock.test.resource { name = 'probe' password = 'public' }\n",
                &format!("provider mock {{ default_tags = {{ Private = secret('{SECRET}') }} }}\n"),
                Failure::ResumeTagEvents,
            )
        },
    );
}

#[test]
fn normalized_resource_events_mask_new_secret_paths() {
    capture(
        "normalized_resource_events_mask_new_secret_paths",
        &[
            "warn [mock] mock.test.resource.(secret) password: (secret)",
            "warn [mock] mock.test.resource.(secret) strings: [(secret)]",
        ],
        || {
            apply_case(
                &format!(
                    "let probe = mock.test.resource {{ name = 'probe' password = secret('{SECRET}') }}\n"
                ),
                "provider mock {}\n",
                Failure::NormalizeEvents,
            )
        },
    );
}
