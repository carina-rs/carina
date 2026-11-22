//! Retained metadata and rejected keys must preserve conversion diagnostics.

use std::{path::Path, process::Command};

use serde_json::{Value, json};

fn conversion_error(message: &str, line: usize, start: usize, end: usize) -> Value {
    json!({
        "message": message,
        "severity": "error",
        "code": "conversion_unsupported_position",
        "location": {
            "file": "main.crn",
            "range": {
                "start": { "line": line, "column": start },
                "end": { "line": line, "column": end },
            },
        },
    })
}

fn assert_conversion_diagnostics(fixture: &str, errors: &[Value], text: &str) {
    let directory = tempfile::tempdir().unwrap();
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/directive_slot_diagnostics")
        .join(fixture);
    for name in ["main.crn", "providers.crn"] {
        std::fs::copy(source.join(name), directory.path().join(name)).unwrap();
    }
    let validate = |json| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carina"));
        command
            .current_dir(directory.path())
            .env("NO_COLOR", "1")
            .env_remove("CLICOLOR_FORCE")
            .args(["validate", "."]);
        if json {
            command.arg("--json");
        }
        command.output().unwrap()
    };
    let output = validate(true);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["errors"], json!(errors), "{document:#}");
    let output = validate(false);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(String::from_utf8(output.stderr).unwrap(), text);
}

#[test]
fn conversion_only_in_directives_keeps_its_placement_diagnostic() {
    assert_conversion_diagnostics(
        "directives-only",
        &[conversion_error(
            "conversion is not supported at this position yet",
            2,
            60,
            98,
        )],
        "Error: ./main.crn:2:60: conversion is not supported at this position yet\n",
    );
}

#[test]
fn rejected_key_keeps_key_and_value_conversion_diagnostics() {
    assert_conversion_diagnostics(
        "rejected-key",
        &[
            conversion_error("conversion is not supported at this position", 4, 7, 50),
            conversion_error("conversion cannot be checked here", 4, 56, 94),
        ],
        "Error: ./main.crn:4:7: conversion is not supported at this position\n\
         Error: ./main.crn:4:56: conversion cannot be checked here\n",
    );
}

#[test]
fn prevent_destroy_mixed_alternatives_keeps_base_diagnostics() {
    assert_conversion_diagnostics(
        "mixed-prevent-destroy",
        &serde_json::from_str::<Vec<Value>>(r#"[
  {
    "message": "./main.crn:1:64: directives.prevent_destroy must resolve to a Bool for resource `mock.test.resource.<pending>`; got a deferred expression",
    "severity": "error",
    "code": null,
    "location": null
  },
  {
    "message": "Attribute 'directives': ?? alternatives mix secret and public values; all alternatives must be secret or all public.",
    "severity": "error",
    "code": null,
    "location": {
      "file": "main.crn",
      "range": {
        "start": {
          "line": 1,
          "column": 77
        },
        "end": {
          "line": 1,
          "column": 79
        }
      }
    }
  }
]"#).unwrap(),
        "Error: ./main.crn:1:64: directives.prevent_destroy must resolve to a Bool for resource `mock.test.resource.<pending>`; got a deferred expression\nError: ./main.crn:1:77: Attribute 'directives': ?? alternatives mix secret and public values; all alternatives must be secret or all public.\n",
    );
}

#[test]
fn force_delete_mixed_alternatives_keeps_base_diagnostics() {
    assert_conversion_diagnostics(
        "mixed-force-delete",
        &serde_json::from_str::<Vec<Value>>(r#"[
  {
    "message": "./main.crn:1:61: directives.force_delete must resolve to a Bool for resource `mock.test.resource.<pending>`; got a deferred expression",
    "severity": "error",
    "code": null,
    "location": null
  },
  {
    "message": "Attribute 'directives': ?? alternatives mix secret and public values; all alternatives must be secret or all public.",
    "severity": "error",
    "code": null,
    "location": {
      "file": "main.crn",
      "range": {
        "start": {
          "line": 1,
          "column": 74
        },
        "end": {
          "line": 1,
          "column": 76
        }
      }
    }
  }
]"#).unwrap(),
        "Error: ./main.crn:1:61: directives.force_delete must resolve to a Bool for resource `mock.test.resource.<pending>`; got a deferred expression\nError: ./main.crn:1:74: Attribute 'directives': ?? alternatives mix secret and public values; all alternatives must be secret or all public.\n",
    );
}

// These pin base diagnostics, including source ranges, for directive thunks.
fn assert_directive_thunk_diagnostics(fixture: &str) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/directive_slot_diagnostics")
        .join(fixture);
    let errors: Vec<Value> =
        serde_json::from_slice(&std::fs::read(source.join("expected-errors.json")).unwrap())
            .unwrap();
    let text = std::fs::read_to_string(source.join("expected-stderr.txt")).unwrap();
    assert_conversion_diagnostics(fixture, &errors, &text);
}

#[test]
fn directives_thunk_direct_reference_keeps_base_diagnostics() {
    assert_directive_thunk_diagnostics("thunk-direct-reference");
}

#[test]
fn directives_thunk_indexed_secret_keeps_base_diagnostics() {
    assert_directive_thunk_diagnostics("thunk-indexed-secret");
}

#[test]
fn directives_thunk_indexed_force_delete_keeps_base_diagnostics() {
    assert_directive_thunk_diagnostics("thunk-indexed-force-delete");
}

#[test]
fn directives_thunk_transitive_reference_keeps_base_diagnostics() {
    assert_directive_thunk_diagnostics("thunk-transitive-reference");
}

#[test]
fn directives_thunk_directive_reference_keeps_base_diagnostics() {
    assert_directive_thunk_diagnostics("thunk-directive-reference");
}

#[test]
fn directives_thunk_self_reference_keeps_base_diagnostics() {
    assert_directive_thunk_diagnostics("thunk-self-reference");
}
