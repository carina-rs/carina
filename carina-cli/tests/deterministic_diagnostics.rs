use std::path::Path;
use std::process::Command;

fn check_fixture(name: &str, expected: &[&str], files: &[&str]) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("carina-core/tests/fixtures/diagnostic_order")
        .join(name);
    for json in [false, true] {
        let mut first = None;
        for _ in 0..8 {
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
            assert_eq!(output.status.code(), Some(1), "{output:?}");
            if json {
                let document: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
                let errors = document["errors"].as_array().unwrap();
                assert_eq!(errors.len(), expected.len(), "{document}");
                for ((error, message), file) in errors.iter().zip(expected).zip(files) {
                    assert!(
                        error["message"].as_str().unwrap().ends_with(message),
                        "{error}"
                    );
                    assert!(
                        error["location"]["file"].as_str().unwrap().ends_with(file),
                        "{error}"
                    );
                }
            } else {
                let stderr = String::from_utf8(output.stderr.clone()).unwrap();
                let mut previous = None;
                for message in expected {
                    assert_eq!(stderr.matches(message).count(), 1, "{stderr}");
                    let position = stderr.find(message).unwrap();
                    assert!(
                        previous.is_none_or(|previous| previous < position),
                        "{stderr}"
                    );
                    previous = Some(position);
                }
            }
            let capture = (output.status.code(), output.stdout, output.stderr);
            if let Some(first) = &first {
                assert_eq!(&capture, first, "{name}, json={json}");
            } else {
                first = Some(capture);
            }
        }
    }
}

#[test]
fn six_unknown_attributes_follow_authored_order() {
    check_fixture(
        "unknown_attributes",
        &[
            "Unknown attribute 'zulu_knob'",
            "Unknown attribute 'alpha_knob'",
            "Unknown attribute 'quartz_knob'",
            "Unknown attribute 'bravo_knob'",
            "Unknown attribute 'mango_knob'",
            "Unknown attribute 'delta_knob'",
        ],
        &["main.crn"; 6],
    );
}

#[test]
fn invalid_function_annotations_follow_sibling_file_order() {
    check_fixture(
        "invalid_annotations",
        &[
            "function parameter 'zebra' parameter 'value': unknown custom type 'mock.test.sdk_validator.Guarded'",
            "function parameter 'alpha' parameter 'value': unknown custom type 'mock.test.sdk_validator.Guarded'",
        ],
        &["a_functions.crn", "z_functions.crn"],
    );
}
