use std::path::Path;
use std::process::Command;

const NAMES: [&str; 6] = ["zulu", "alpha", "quartz", "bravo", "mango", "delta"];

fn check_case(case: &str) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("carina-core/tests/fixtures/diagnostic_order/module_arguments")
        .join(case);
    let file = directory.join("main.crn");
    let mut errors = Vec::new();
    let mut stderr = String::new();
    if case == "references" {
        for name in NAMES {
            let message = format!(
                "Undefined identifier `missing_{name}`. In-scope names: choice, x (let / upstream_state / read / module / function / for / fn / arguments)"
            );
            stderr.push_str(&format!("Error: {message}\n"));
            errors.push(serde_json::json!({"message": message, "severity": "error", "code": null, "location": null}));
        }
    }
    for (index, name) in NAMES.into_iter().enumerate() {
        let argument = format!(
            "{name}_{}",
            if case == "references" { "ref" } else { "bad" }
        );
        let message = format!("Unknown argument '{argument}' for module 'choice'");
        stderr.push_str(&format!(
            "Error: {} at line {}\nError: {message}\n",
            file.display(),
            index + 3
        ));
        errors.push(
            serde_json::json!({"message": message, "severity": "error", "code": null,
            "location": {"file": file, "range": {"start": {"line": index + 3, "column": 3},
                "end": {"line": index + 3, "column": argument.len() + 3}}}}),
        );
    }
    for _ in 0..8 {
        for json in [false, true] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
            command
                .arg("validate")
                .arg(&directory)
                .env("NO_COLOR", "1")
                .env_remove("CLICOLOR_FORCE");
            if json {
                command.arg("--json");
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), Some(1));
            if json {
                assert!(output.stderr.is_empty(), "{output:?}");
                let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(
                    result,
                    serde_json::json!({"status": "error", "resource_count": 0, "resources": [], "errors": errors})
                );
            } else {
                assert_eq!(output.stdout, b"Validating...\n");
                assert_eq!(String::from_utf8(output.stderr).unwrap(), stderr);
            }
        }
    }
}

#[test]
fn unknown_module_arguments_follow_authored_order_in_text_and_json() {
    check_case("unknown");
}

#[test]
fn undefined_module_argument_references_follow_authored_order_in_text_and_json() {
    check_case("references");
}

#[test]
fn undefined_module_argument_functions_fail_at_the_first_authored_call() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("carina-core/tests/fixtures/diagnostic_order/module_arguments/functions");
    let file = directory.join("main.crn");
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .arg("validate")
            .arg(&directory)
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE");
        if json {
            command.arg("--json");
        }
        let output = command.output().unwrap();
        assert_eq!(output.status.code(), Some(1));
        if json {
            let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(
                result,
                serde_json::json!({"status": "error", "resource_count": 0, "resources": [], "errors": [{
                    "message": "Unknown function 'missing_zulu'", "severity": "error", "code": null,
                    "location": {"file": file, "range": {"start": {"line": 3, "column": 15}, "end": {"line": 3, "column": 27}}}
                }]})
            );
            assert!(output.stderr.is_empty());
        } else {
            assert!(output.stdout.is_empty());
            assert_eq!(
                String::from_utf8(output.stderr).unwrap(),
                format!(
                    "Error: {}: Invalid expression at line 3: Unknown function 'missing_zulu'\n",
                    file.display()
                )
            );
        }
    }
}
