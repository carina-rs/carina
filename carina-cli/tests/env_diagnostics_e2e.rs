//! Environment strings are public unless they pass through secret() (#4409).

use std::process::Command;
use std::sync::Arc;

use carina_core::EnvironmentSnapshot;
use carina_core::provider::{ProviderFactory, loaded_provider_snapshot_from_factories};
use carina_core::schema::SchemaRegistry;
use carina_lsp::{diagnostics::DiagnosticEngine, document::Document};

#[test]
fn env_validation_directory_cli_text_json_and_lsp_preserves_secret_boundary() {
    const VARIABLE: &str = "CARINA_4409_DIAGNOSTIC_VALUE";
    const VALUE: &str = "env-diagnostic-value";
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("providers.crn"), "provider mock {}\n").unwrap();
    let path = directory.path().join("main.crn");
    let environment = EnvironmentSnapshot::new([(VARIABLE.to_string(), VALUE.into())]);
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let providers = loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        environment.clone(),
    );
    let context = Arc::new(providers.provider_context().as_provider_context().clone());
    let engine = DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]);

    // Exercise the secret control before the ordinary env case, so a red run
    // also proves that all three surfaces already protect secret(env(...)).
    for (expression, hidden) in [
        (format!("secret(env('{VARIABLE}'))"), true),
        (format!("env('{VARIABLE}')"), false),
    ] {
        let source = format!("mock.test.pattern_modes {{\n  full = {expression}\n}}\n");
        std::fs::write(&path, &source).unwrap();
        let run = |json| {
            let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .current_dir(directory.path())
                .args(["validate", "."])
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .env(VARIABLE, VALUE);
            if json {
                command.arg("--json");
            }
            command.output().unwrap()
        };
        let human = run(false);
        let json = run(true);
        assert_eq!(human.status.code(), Some(1));
        assert_eq!(json.status.code(), Some(1));
        assert!(json.stderr.is_empty(), "{json:?}");
        let report: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
        let errors = report["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 1, "{report}");
        let message = errors[0]["message"].as_str().unwrap();
        let human = format!(
            "{}{}",
            String::from_utf8_lossy(&human.stdout),
            String::from_utf8_lossy(&human.stderr)
        );
        assert!(human.contains(message), "{human}\n{report}");

        let provider_path = directory.path().join("providers.crn");
        let provider_document = Document::new_with_path(
            std::fs::read_to_string(&provider_path).unwrap(),
            provider_path.clone(),
            context.clone(),
        );
        assert!(
            engine
                .analyze(&provider_document, Some(&provider_path))
                .is_empty()
        );
        let document = Document::new_with_path(source, path.clone(), context.clone());
        let diagnostics = engine.analyze(&document, Some(&path));
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        assert_eq!(
            message, diagnostics[0].message,
            "CLI={report}\nLSP={diagnostics:#?}"
        );
        let expected = if hidden {
            "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
        } else {
            "Invalid value 'env-diagnostic-value' for 'full': does not match required pattern /abc/ (Full)"
        };
        assert_eq!(message, expected);

        for (surface, rendered) in [
            ("CLI text", human),
            ("CLI JSON", String::from_utf8(json.stdout).unwrap()),
            ("LSP", format!("{diagnostics:#?}")),
        ] {
            assert_eq!(rendered.contains(VALUE), !hidden, "{surface}: {rendered}");
            assert!(rendered.contains(expected), "{surface}: {rendered}");
        }
    }
}

#[test]
fn real_secret_directory_reachability_preserves_diagnostic_boundaries() {
    #[derive(serde::Deserialize)]
    struct ExpectedCase {
        validation_codes: Vec<Option<i32>>,
        errors: Vec<String>,
        lsp: Vec<String>,
        runtime: Option<[i32; 2]>,
        plan_errors: Vec<String>,
        apply_errors: Vec<String>,
    }
    let mut expected: std::collections::BTreeMap<String, ExpectedCase> = serde_json::from_str(r#"{
  "literal": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "interpolation": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "upper": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "join": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)",
      "Consider using pipe form for 'join': data |> join(...)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "concat": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)",
      "Consider using pipe form for 'concat': data |> concat(...)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "list": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "map": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "secret_map": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "secret_subscript": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "index access key must be an integer or string, got secret"
    ],
    "lsp": [
      "index access key must be an integer or string, got secret"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "deferred_subscript": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Undefined identifier `(secret)`. In-scope names: identity (let / upstream_state / read / module / function / for / fn / arguments)"
    ],
    "lsp": [
      "invalid value inside secret(...)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "deferred_binding": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid expression"
    ],
    "lsp": [
      "Invalid expression"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "for_list": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [],
    "runtime": [
      0,
      0
    ],
    "plan_errors": [],
    "apply_errors": []
  },
  "for_map": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [
      "for-loop binding 'value' is unused. Rename to '_' to suppress this warning."
    ],
    "runtime": [
      0,
      0
    ],
    "plan_errors": [],
    "apply_errors": []
  },
  "for_bound_list": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [],
    "runtime": [
      0,
      0
    ],
    "plan_errors": [],
    "apply_errors": []
  },
  "for_bound_map": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [
      "for-loop binding 'value' is unused. Rename to '_' to suppress this warning."
    ],
    "runtime": [
      0,
      0
    ],
    "plan_errors": [],
    "apply_errors": []
  },
  "module_argument": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "export": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "export 'value' cannot hold a secret value"
    ],
    "lsp": [
      "export 'value' cannot hold a secret value"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "provider_attribute": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "provider mock: token: Unknown attribute 'token'"
    ],
    "lsp": [
      "provider mock: token: Unknown attribute 'token'"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "default_tags": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Finalize error: Invalid expression at line 0: Provider 'mock': default_tags must resolve to a map, got Deferred(Secret((secret)))"
    ],
    "lsp": [
      "Invalid expression at line 0: Provider 'mock': default_tags must resolve to a map, got Deferred(Secret((secret)))"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "backend_attribute": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid expression at line 1: backend s3 attribute 'bucket' must be a plain string or other non-secret value; secret()- and decrypt()-derived values are not allowed"
    ],
    "lsp": [
      "Invalid expression at line 1: backend s3 attribute 'bucket' must be a plain string or other non-secret value; secret()- and decrypt()-derived values are not allowed",
      "Invalid expression at line 1: backend s3 attribute 'bucket' must be a plain string or other non-secret value; secret()- and decrypt()-derived values are not allowed",
      "Invalid expression at line 1: backend s3 attribute 'bucket' must be a plain string or other non-secret value; secret()- and decrypt()-derived values are not allowed"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "argument_default": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Invalid value (secret): does not match required pattern /abc/ (Full)"
    ],
    "lsp": [
      "mock.test.pattern_modes.<pending>: Invalid value (secret) for 'full': does not match required pattern /abc/ (Full)"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "provider_deferred_subscript": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "Undefined identifier `(secret)`. In-scope names: value (let / upstream_state / read / module / function / for / fn / arguments)",
      "provider mock: reference_table: Unknown attribute 'reference_table'"
    ],
    "lsp": [
      "invalid value inside secret(...)",
      "provider mock: reference_table: Unknown attribute 'reference_table'"
    ],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "default_tag_value": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [],
    "runtime": null,
    "plan_errors": [],
    "apply_errors": []
  },
  "deferred_missing_map_key": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [],
    "runtime": [
      0,
      1
    ],
    "plan_errors": [],
    "apply_errors": [
      "→ Attribute 'comment': invalid value inside secret(...)",
      "caused by: invalid value inside secret(...)",
      "Error: Apply failed. 1 succeeded, 1 failed.",
      "Error:   create mock.test.resource.mock_test_resource_e2fb2255: execution error"
    ]
  },
  "runtime_secret_reference_for": {
    "validation_codes": [
      1,
      1
    ],
    "errors": [
      "deferred-for iterable `producer.comment`: expected list iterable, got String"
    ],
    "lsp": [
      "deferred-for iterable `producer.comment`: expected list iterable, got String"
    ],
    "runtime": [
      1,
      1
    ],
    "plan_errors": ["Error: ./main.crn:2:14: deferred-for iterable `producer.comment`: expected list iterable, got String"],
    "apply_errors": ["Error: ./main.crn:2:14: deferred-for iterable `producer.comment`: expected list iterable, got String"]
  },
  "runtime_secret_attribute": {
    "validation_codes": [
      0,
      0
    ],
    "errors": [],
    "lsp": [],
    "runtime": [
      0,
      0
    ],
    "plan_errors": [],
    "apply_errors": []
  }
}"#).unwrap();
    const CANARY: &str = "CANARY_4409_SECRET";
    let environment = EnvironmentSnapshot::empty();
    let factories: Vec<Box<dyn ProviderFactory>> =
        vec![Box::new(carina_provider_mock::ConfiguredBuiltinMock::new(
            Default::default(),
            carina_provider_mock::MockScope::memory(),
        ))];
    let providers = loaded_provider_snapshot_from_factories(
        Arc::new(factories),
        SchemaRegistry::new(),
        environment,
    );
    let context = Arc::new(providers.provider_context().as_provider_context().clone());
    let engine = DiagnosticEngine::new(Arc::new(providers), vec!["mock".into()]);
    let mut cases = Vec::new();
    for (label, expression) in [
        ("literal", "secret('CANARY_4409_SECRET')"),
        ("interpolation", "\"${secret('CANARY_4409_SECRET')}\""),
        ("upper", "upper(secret('CANARY_4409_SECRET'))"),
        ("join", "join('', [secret('CANARY_4409_SECRET')])"),
        ("concat", "concat([secret('CANARY_4409_SECRET')], [])[0]"),
        ("list", "[secret('CANARY_4409_SECRET')][0]"),
        ("map", "{ k = secret('CANARY_4409_SECRET') }.k"),
        ("secret_map", "secret({ k = 'CANARY_4409_SECRET' }).k"),
        ("secret_subscript", "missing[secret('CANARY_4409_SECRET')]"),
        (
            "deferred_subscript",
            "secret(missing.value['CANARY_4409_SECRET'])",
        ),
        (
            "deferred_binding",
            "secret(identity(missing)['CANARY_4409_SECRET'])",
        ),
    ] {
        cases.push((
            label,
            format!("fn identity(x) {{ x }}\nmock.test.pattern_modes {{ full = {expression} }}\n"),
            "provider mock {}\n",
        ));
    }
    cases.extend([
        ("for_list", "for value in secret(['CANARY_4409_SECRET']) { mock.test.pattern_modes { full = value } }\n".into(), "provider mock {}\n"),
        ("for_map", "for key, value in secret({ CANARY_4409_SECRET = 'public' }) { mock.test.pattern_modes { full = key } }\n".into(), "provider mock {}\n"),
        ("for_bound_list", "let rows = for value in secret(['CANARY_4409_SECRET']) { mock.test.pattern_modes { full = value } }\n".into(), "provider mock {}\n"),
        ("for_bound_map", "let rows = for key, value in secret({ CANARY_4409_SECRET = 'public' }) { mock.test.pattern_modes { full = key } }\n".into(), "provider mock {}\n"),
        ("module_argument", "let m = use { source = './m' }\nm { value = secret('CANARY_4409_SECRET') }\n".into(), "provider mock {}\n"),
        ("export", "exports { value: String = secret('CANARY_4409_SECRET') }\n".into(), "provider mock {}\n"),
        ("provider_attribute", "let value = 'public'\n".into(), "provider mock { token = secret('CANARY_4409_SECRET') }\n"),
        ("default_tags", "let value = 'public'\n".into(), "provider mock { default_tags = secret({ Key = 'CANARY_4409_SECRET' }) }\n"),
        ("backend_attribute", "backend s3 { bucket = secret('CANARY_4409_SECRET') key = 'state.json' }\n".into(), "provider mock {}\n"),
        ("argument_default", "let m = use { source = './m' }\nm {}\n".into(), "provider mock {}\n"),
        ("provider_deferred_subscript", "let value = 'public'\n".into(), "provider mock { reference_table = secret(missing.value['CANARY_4409_SECRET']) }\n"),
        ("default_tag_value", "mock.test.resource { name = 'tags' }\n".into(), "provider mock { default_tags = { Key = secret('CANARY_4409_SECRET') } }\n"),
        ("deferred_missing_map_key", "let producer = mock.test.resource { name = 'producer' tags = { available = 'public' } }\nmock.test.resource { name = 'consumer' comment = secret(producer.tags['CANARY_4409_SECRET']) }\n".into(), "provider mock {}\n"),
        ("runtime_secret_reference_for", "let producer = mock.test.resource { name = 'producer' comment = secret('CANARY_4409_SECRET') }\nfor value in producer.comment { mock.test.pattern_modes { full = value } }\n".into(), "provider mock {}\n"),
        ("runtime_secret_attribute", "mock.test.resource { name = 'private-input' comment = secret('CANARY_4409_SECRET') }\n".into(), "provider mock {}\n"),
    ]);
    for (label, source, provider_source) in cases {
        let expected_case = expected
            .remove(label)
            .expect("every source case has exact expectations");
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("main.crn"), &source).unwrap();
        std::fs::write(directory.path().join("providers.crn"), provider_source).unwrap();
        if label.starts_with("for_")
            || (label == "deferred_missing_map_key" || label.starts_with("runtime_"))
        {
            std::fs::write(
                directory.path().join("backend.crn"),
                "backend local { path = 'state.json' }\n",
            )
            .unwrap();
        }
        std::fs::create_dir(directory.path().join("m")).unwrap();
        let module_source = if label == "argument_default" {
            "arguments { value: String = secret('CANARY_4409_SECRET') }\nmock.test.pattern_modes { full = value }\n"
        } else {
            "arguments { value: String }\n"
        };
        std::fs::write(directory.path().join("m/main.crn"), module_source).unwrap();
        let mut messages = Vec::new();
        let mut statuses = Vec::new();
        for json in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .current_dir(directory.path())
                .args(["validate", "."])
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE");
            if json {
                command.arg("--json");
            }
            let output = command.output().unwrap();
            let rendered = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !rendered
                    .to_ascii_lowercase()
                    .contains(&CANARY.to_ascii_lowercase()),
                "masked value was shown: {label}, json={json}: {rendered}"
            );
            statuses.push(output.status.code());
            if json {
                let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                if let Some(errors) = report["errors"].as_array() {
                    messages.extend(
                        errors
                            .iter()
                            .map(|error| error["message"].as_str().unwrap().to_string()),
                    );
                } else {
                    assert!(
                        output.status.success(),
                        "{label}: unexpected report {report}"
                    );
                }
            }
        }
        let mut lsp_messages = Vec::new();
        if label.starts_with("for_")
            || (label == "deferred_missing_map_key" || label.starts_with("runtime_"))
        {
            let initialized = Command::new(env!("CARGO_BIN_EXE_carina"))
                .current_dir(directory.path())
                .args(["init", "."])
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .output()
                .unwrap();
            let initialized_text = format!(
                "{}{}",
                String::from_utf8_lossy(&initialized.stdout),
                String::from_utf8_lossy(&initialized.stderr)
            );
            assert!(
                !initialized_text
                    .to_ascii_lowercase()
                    .contains(&CANARY.to_ascii_lowercase()),
                "masked value was shown: {label}, init: {initialized_text}"
            );
            assert!(initialized.status.success(), "{label}: {initialized_text}");
            let output = Command::new(env!("CARGO_BIN_EXE_carina"))
                .current_dir(directory.path())
                .args(["plan", ".", "--refresh=false", "--out", "plan.json"])
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .output()
                .unwrap();
            let rendered = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !rendered
                    .to_ascii_lowercase()
                    .contains(&CANARY.to_ascii_lowercase()),
                "masked value was shown: {label}, plan: {rendered}"
            );
            let runtime = expected_case
                .runtime
                .expect("runtime case has exit status expectations");
            assert_eq!(
                output.status.code(),
                Some(runtime[0]),
                "{label}: {rendered}"
            );
            let plan_errors = rendered
                .lines()
                .map(str::trim_start)
                .filter(|line| {
                    line.starts_with("Error:")
                        || line.starts_with("→ Attribute")
                        || line.starts_with("caused by:")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                plan_errors, expected_case.plan_errors,
                "{label}: {rendered}"
            );
            if label.starts_with("for_") {
                assert!(
                    rendered.contains("N records after (secret) resolves"),
                    "{label}: {rendered}"
                );
            }
            let plan_path = directory.path().join("plan.json");
            if runtime[0] == 0 {
                let plan = std::fs::read_to_string(plan_path).unwrap();
                assert!(
                    !plan
                        .to_ascii_lowercase()
                        .contains(&CANARY.to_ascii_lowercase()),
                    "masked value was shown in plan file: {label}: {plan}"
                );
            }
            let applied = Command::new(env!("CARGO_BIN_EXE_carina"))
                .current_dir(directory.path())
                .args(["apply", ".", "--auto-approve"])
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE")
                .output()
                .unwrap();
            let applied_text = format!(
                "{}{}",
                String::from_utf8_lossy(&applied.stdout),
                String::from_utf8_lossy(&applied.stderr)
            );
            assert!(
                !applied_text
                    .to_ascii_lowercase()
                    .contains(&CANARY.to_ascii_lowercase()),
                "masked value was shown: {label}, apply: {applied_text}"
            );
            assert_eq!(
                applied.status.code(),
                Some(runtime[1]),
                "{label}: {applied_text}"
            );
            let apply_errors = applied_text
                .lines()
                .map(str::trim_start)
                .filter(|line| {
                    line.starts_with("Error:")
                        || line.starts_with("→ Attribute")
                        || line.starts_with("caused by:")
                })
                .collect::<Vec<_>>();
            assert_eq!(
                apply_errors, expected_case.apply_errors,
                "{label}: {applied_text}"
            );
            if label.starts_with("for_") {
                assert!(
                    applied_text.contains("No changes needed."),
                    "{label}: {applied_text}"
                );
            }
            let state_path = directory.path().join("state.json");
            if state_path.exists() {
                let state = std::fs::read_to_string(&state_path).unwrap();
                assert!(
                    !state
                        .to_ascii_lowercase()
                        .contains(&CANARY.to_ascii_lowercase()),
                    "masked value was shown in state file: {label}: {state}"
                );
            }
        }
        for filename in ["providers.crn", "main.crn"] {
            let path = directory.path().join(filename);
            let document = Document::new_with_path(
                std::fs::read_to_string(&path).unwrap(),
                path.clone(),
                context.clone(),
            );
            let diagnostics = engine.analyze(&document, Some(&path));
            let rendered = format!("{diagnostics:#?}");
            assert!(
                !rendered
                    .to_ascii_lowercase()
                    .contains(&CANARY.to_ascii_lowercase()),
                "masked value was shown: {label}, LSP {filename}: {rendered}"
            );
            lsp_messages.extend(diagnostics.into_iter().map(|diagnostic| diagnostic.message));
        }
        assert_eq!(statuses, expected_case.validation_codes, "{label}");
        assert_eq!(messages, expected_case.errors, "CLI JSON: {label}");
        assert_eq!(lsp_messages, expected_case.lsp, "LSP: {label}");
    }
    assert!(expected.is_empty(), "every expected case must run");
}
